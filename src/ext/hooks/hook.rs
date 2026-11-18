use std::time::Duration;

use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use reqwest::Url;
use serde_json::Value;

use crate::ext::hooks::hook_reply::{Effect, HookReply};
use crate::ext::hooks::hook_spec::{FailMode, HookSpec, SendPart};
use crate::ext::rules::{Matcher, PhaseName, RuleMode, expand};

/// The longest reply the hub reads from a hook.
const MAX_REPLY_BYTES: usize = 1024 * 1024;
/// The time a hook gets when it names none.
const DEFAULT_TIMEOUT: Duration = Duration::from_millis(300);
/// The longest time a hook may ask for. A hook runs on the request path.
const MAX_TIMEOUT_MS: u64 = 5_000;

/// One compiled hook.
#[derive(Debug)]
pub(crate) struct Hook {
    pub name: String,
    pub phase: PhaseName,
    pub url: Url,
    pub matcher: Matcher,
    pub send_body: bool,
    pub timeout: Duration,
    pub fail_closed: bool,
    pub shadow: bool,
}

impl Hook {
    /// Compiles `spec`. The error names the hook and what is wrong with it.
    pub(crate) fn compile(
        spec: HookSpec,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Hook, String> {
        let name = spec.name.trim().to_owned();
        if name.is_empty() {
            return Err("a hook has an empty name".to_owned());
        }
        let fail = |reason: String| format!("hook `{name}`: {reason}");
        if !matches!(
            spec.phase,
            PhaseName::Request | PhaseName::Attempt | PhaseName::Complete
        ) {
            return Err(fail(format!(
                "the {} phase does not run hooks. Use request, attempt or complete",
                spec.phase.as_str()
            )));
        }
        let send_body = spec.send.contains(&SendPart::Body);
        if spec.phase == PhaseName::Complete && !spec.send.is_empty() {
            return Err(fail(
                "the complete phase sends the outcome only, so it takes no `send`".to_owned(),
            ));
        }
        let timeout = match spec.timeout_ms {
            None => DEFAULT_TIMEOUT,
            Some(ms) if (1..=MAX_TIMEOUT_MS).contains(&ms) => Duration::from_millis(ms),
            Some(ms) => {
                return Err(fail(format!(
                    "timeout_ms {ms} is not between 1 and {MAX_TIMEOUT_MS}"
                )));
            }
        };
        Ok(Hook {
            url: checked_url(&expand(&spec.url, env).map_err(&fail)?).map_err(&fail)?,
            matcher: Matcher::compile(spec.matches, spec.phase).map_err(&fail)?,
            phase: spec.phase,
            send_body,
            timeout,
            fail_closed: spec.fail == FailMode::Closed,
            shadow: spec.mode == RuleMode::Shadow,
            name,
        })
    }

    /// Posts `payload` and reads the hook's answer. An empty 2xx answer means continue.
    pub(crate) async fn invoke(
        &self,
        client: &reqwest::Client,
        payload: &Value,
    ) -> Result<Effect, String> {
        let response = client
            .post(self.url.clone())
            .timeout(self.timeout)
            .json(payload)
            .send()
            .await
            .map_err(|e| format!("the call failed: {e}"))?;
        if !response.status().is_success() {
            return Err(format!("the hook answered HTTP {}", response.status()));
        }
        let raw = read_capped(response).await?;
        if raw.iter().all(u8::is_ascii_whitespace) {
            return Ok(Effect::Continue);
        }
        serde_json::from_slice::<HookReply>(&raw)
            .map_err(|e| format!("the reply is not valid: {e}"))?
            .into_effect()
    }
}

/// `https`, or `http` on this machine only: a hook sees request data.
fn checked_url(text: &str) -> Result<Url, String> {
    let url = Url::parse(text).map_err(|e| format!("`{text}` is not a URL: {e}"))?;
    match (url.scheme(), url.host_str()) {
        ("https", Some(_)) => Ok(url),
        ("http", Some(host)) if is_loopback(host) => Ok(url),
        ("http", Some(_)) => Err(format!(
            "`{text}` is plain http to another machine. Use https, or http on localhost"
        )),
        _ => Err(format!("`{text}` is not an http or https URL")),
    }
}

fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

async fn read_capped(response: reqwest::Response) -> Result<Bytes, String> {
    let mut chunks = response.bytes_stream();
    let mut read = BytesMut::new();
    while let Some(chunk) = chunks.next().await {
        read.extend_from_slice(&chunk.map_err(|e| format!("the reply failed to read: {e}"))?);
        if read.len() > MAX_REPLY_BYTES {
            return Err(format!("the reply is over {MAX_REPLY_BYTES} bytes"));
        }
    }
    Ok(read.freeze())
}
