//! Native local-process probe for hub-owned models.

use std::time::Duration;

use reqwest::StatusCode;
use serde::Deserialize;

use crate::services::local_serving::pool::LocalModelSnapshot;

const READ_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NativeEvidence {
    pub ctx: Option<u64>,
}

#[derive(Default, Deserialize)]
struct Props {
    #[serde(default)]
    n_ctx: Option<u64>,
}

pub async fn probe(client: &reqwest::Client, model: &LocalModelSnapshot) -> NativeEvidence {
    let Some(port) = model.port else {
        return NativeEvidence::default();
    };
    let props = get::<Props>(client, &format!("http://127.0.0.1:{port}/props"))
        .await
        .ok();
    NativeEvidence {
        ctx: props.and_then(|value| value.n_ctx),
    }
}

async fn get<T: for<'de> Deserialize<'de>>(
    client: &reqwest::Client,
    url: &str,
) -> Result<T, String> {
    let response = client
        .get(url)
        .timeout(READ_TIMEOUT)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let status = response.status();
    if status != StatusCode::OK {
        return Err(status.to_string());
    }
    response
        .json::<T>()
        .await
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_port_has_no_native_evidence() {
        let model = LocalModelSnapshot {
            profile: "local".into(),
            id: "a".into(),
            state: crate::services::local_serving::pool::LocalModelState::Stopped,
            port: None,
            pid: None,
            argv: Vec::new(),
            engine: None,
            started: None,
            last_used: std::time::Instant::now(),
            in_flight: 0,
            estimate_gb: 0.0,
        };
        assert_eq!(NativeEvidence::default(), NativeEvidence { ctx: None });
        assert_eq!(model.port, None);
    }
}
