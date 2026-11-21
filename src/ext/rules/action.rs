use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use serde_json::{Map, Value};

use crate::consts::HEADER_FALLBACKS;
use crate::ext::rules::matcher::{header_name, header_value};
use crate::ext::rules::rule_spec::{ActionSpec, NotifySeverity, NotifySpec, PhaseName};
use crate::ext::{
    AttemptCtx, ExtNotification, ExtSeverity, Flow, HttpRequestCtx, HttpResponseCtx, RequestCtx,
    ResponseCtx,
};

/// One compiled rule action.
#[derive(Debug)]
pub(crate) enum Action {
    SetHeader(Vec<(HeaderName, HeaderValue)>),
    RemoveHeader(Vec<HeaderName>),
    SetField(Vec<(String, Value)>),
    RemoveField(Vec<String>),
    SetModel(String),
    Reject(StatusCode, String),
    RetryNext,
    SetStatus(StatusCode),
    Notify(ExtNotification),
}

impl Action {
    /// Compiles `spec` for `phase`. `env` resolves `${NAME}` in strings.
    pub fn compile(
        spec: ActionSpec,
        phase: PhaseName,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Action, String> {
        let name = spec.name();
        let allowed = allowed_actions(phase);
        if !allowed.contains(&name) {
            return Err(format!(
                "action `{name}` is not available in the {} phase",
                phase.as_str()
            ));
        }
        Ok(match spec {
            ActionSpec::SetHeader(pairs) => Action::SetHeader(
                pairs
                    .into_iter()
                    .map(|(name, value)| {
                        Ok((header_name(&name)?, header_value(&expand(&value, env)?)?))
                    })
                    .collect::<Result<_, String>>()?,
            ),
            ActionSpec::RemoveHeader(names) => Action::RemoveHeader(
                names
                    .into_vec()
                    .iter()
                    .map(|name| header_name(name))
                    .collect::<Result<_, _>>()?,
            ),
            ActionSpec::SetField(fields) => Action::SetField(
                fields
                    .into_iter()
                    .map(|(pointer, value)| {
                        Ok((checked_pointer(pointer)?, expand_value(value, env)?))
                    })
                    .collect::<Result<_, String>>()?,
            ),
            ActionSpec::RemoveField(pointers) => Action::RemoveField(
                pointers
                    .into_vec()
                    .into_iter()
                    .map(checked_pointer)
                    .collect::<Result<_, _>>()?,
            ),
            ActionSpec::SetModel(model) => Action::SetModel(expand(&model, env)?),
            ActionSpec::SetFallbacks(models) => {
                let chain = models
                    .iter()
                    .map(|m| expand(m, env))
                    .collect::<Result<Vec<_>, _>>()?;
                Action::SetHeader(vec![(
                    HeaderName::from_static(HEADER_FALLBACKS),
                    header_value(&chain.join(","))?,
                )])
            }
            ActionSpec::Reject(reject) => Action::Reject(
                StatusCode::from_u16(reject.status)
                    .ok()
                    .filter(|status| status.as_u16() >= 400)
                    .ok_or_else(|| {
                        format!("reject status {} is not a 4xx or 5xx status", reject.status)
                    })?,
                expand(&reject.message, env)?,
            ),
            ActionSpec::RetryNext(true) => Action::RetryNext,
            ActionSpec::RetryNext(false) => return Err("retry_next takes `true`".to_owned()),
            ActionSpec::SetStatus(status) => Action::SetStatus(
                StatusCode::from_u16(status)
                    .ok()
                    .filter(|s| s.as_u16() >= 200)
                    .ok_or_else(|| format!("set_status {status} is not an HTTP status"))?,
            ),
            ActionSpec::Notify(notify) => Action::Notify(compile_notify(&notify, env)?),
        })
    }

    /// The action as a short phrase for a log line, such as `reject(403)`.
    pub fn describe(&self) -> String {
        match self {
            Action::SetHeader(_) => "set_header".to_owned(),
            Action::RemoveHeader(_) => "remove_header".to_owned(),
            Action::SetField(_) => "set_field".to_owned(),
            Action::RemoveField(_) => "remove_field".to_owned(),
            Action::SetModel(_) => "set_model".to_owned(),
            Action::Reject(status, _) => format!("reject({})", status.as_u16()),
            Action::RetryNext => "retry_next".to_owned(),
            Action::SetStatus(status) => format!("set_status({})", status.as_u16()),
            Action::Notify(_) => "notify".to_owned(),
        }
    }

    /// True when the action changes a JSON body.
    pub fn changes_body(&self) -> bool {
        matches!(
            self,
            Action::SetField(_) | Action::RemoveField(_) | Action::SetModel(_)
        )
    }

    /// Applies a header, body or reject action to `target`. Returns the rejection when the action
    /// rejects. The response-only actions are left to the caller.
    pub fn apply_to(&self, target: &mut impl ActionTarget) -> Option<Flow> {
        if let Action::Notify(note) = self {
            target.target_notify(note.clone());
            return None;
        }
        if let Action::Reject(status, message) = self {
            return Some(Flow::reject(status.as_u16(), message.clone()));
        }
        apply_headers(self, target.target_headers());
        if self.changes_body()
            && let Some(body) = target.target_body()
        {
            apply_body(self, body);
        }
        None
    }
}

/// The actions a rule in `phase` may run.
fn allowed_actions(phase: PhaseName) -> &'static [&'static str] {
    match phase {
        PhaseName::HttpRequest => &["set_header", "remove_header", "reject", "notify"],
        PhaseName::HttpResponse => &["set_header", "remove_header", "set_status", "notify"],
        PhaseName::Complete => &[],
        PhaseName::Request => &[
            "set_header",
            "remove_header",
            "set_field",
            "remove_field",
            "set_model",
            "set_fallbacks",
            "reject",
            "notify",
        ],
        PhaseName::Attempt => &[
            "set_header",
            "remove_header",
            "set_field",
            "remove_field",
            "set_model",
            "reject",
            "notify",
        ],
        PhaseName::Response => &[
            "set_header",
            "remove_header",
            "set_field",
            "remove_field",
            "retry_next",
            "set_status",
            "notify",
        ],
    }
}

/// The notification of a `notify` action, with `${NAME}` expanded in its title and body.
fn compile_notify(
    notify: &NotifySpec,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<ExtNotification, String> {
    let title = expand(notify.title.trim(), env)?;
    if title.is_empty() {
        return Err("notify needs a title".to_owned());
    }
    let severity = match notify.severity {
        NotifySeverity::Info => ExtSeverity::Info,
        NotifySeverity::Warning => ExtSeverity::Warning,
        NotifySeverity::Critical => ExtSeverity::Critical,
    };
    Ok(ExtNotification::new(
        severity,
        title,
        expand(&notify.body, env)?,
    ))
}

/// What a rule action changes: the headers and the JSON body of one phase, and where its
/// notifications go.
pub(crate) trait ActionTarget {
    fn target_headers(&mut self) -> &mut HeaderMap;
    fn target_body(&mut self) -> Option<&mut Value>;
    /// Sends `note`, with the model and profile this phase knows when the note names none.
    fn target_notify(&mut self, note: ExtNotification);
}

impl ActionTarget for RequestCtx<'_> {
    fn target_headers(&mut self) -> &mut HeaderMap {
        self.headers_mut()
    }

    fn target_body(&mut self) -> Option<&mut Value> {
        self.body_mut()
    }

    fn target_notify(&mut self, mut note: ExtNotification) {
        if note.model.is_none() {
            note.model = self.model().map(str::to_owned);
        }
        self.notify(note);
    }
}

impl ActionTarget for AttemptCtx<'_> {
    fn target_headers(&mut self) -> &mut HeaderMap {
        self.headers_mut()
    }

    fn target_body(&mut self) -> Option<&mut Value> {
        self.body_mut()
    }

    fn target_notify(&mut self, note: ExtNotification) {
        let note = with_target(note, self.profile(), self.model());
        self.notify(note);
    }
}

impl ActionTarget for HttpRequestCtx<'_> {
    fn target_headers(&mut self) -> &mut HeaderMap {
        self.headers_mut()
    }

    fn target_body(&mut self) -> Option<&mut Value> {
        None
    }

    fn target_notify(&mut self, note: ExtNotification) {
        self.notify(note);
    }
}

impl ActionTarget for HttpResponseCtx<'_> {
    fn target_headers(&mut self) -> &mut HeaderMap {
        self.headers_mut()
    }

    fn target_body(&mut self) -> Option<&mut Value> {
        None
    }

    fn target_notify(&mut self, note: ExtNotification) {
        self.notify(note);
    }
}

impl ActionTarget for ResponseCtx<'_> {
    fn target_headers(&mut self) -> &mut HeaderMap {
        self.headers_mut()
    }

    fn target_body(&mut self) -> Option<&mut Value> {
        self.error_body_mut()
    }

    fn target_notify(&mut self, note: ExtNotification) {
        let note = with_target(note, self.profile(), self.model());
        self.notify(note);
    }
}

/// `note` with `profile` and `model` where it names none.
fn with_target(mut note: ExtNotification, profile: &str, model: &str) -> ExtNotification {
    note.profile.get_or_insert_with(|| profile.to_owned());
    note.model.get_or_insert_with(|| model.to_owned());
    note
}

/// Applies the header actions to `headers`.
pub(crate) fn apply_headers(action: &Action, headers: &mut HeaderMap) {
    match action {
        Action::SetHeader(pairs) => {
            for (name, value) in pairs {
                headers.insert(name.clone(), value.clone());
            }
        }
        Action::RemoveHeader(names) => {
            for name in names {
                headers.remove(name);
            }
        }
        _ => {}
    }
}

/// Applies the body actions to `body`.
pub(crate) fn apply_body(action: &Action, body: &mut Value) {
    match action {
        Action::SetField(fields) => {
            for (pointer, value) in fields {
                set_pointer(body, pointer, value.clone());
            }
        }
        Action::RemoveField(pointers) => {
            for pointer in pointers {
                remove_pointer(body, pointer);
            }
        }
        Action::SetModel(model) => set_pointer(body, "/model", Value::from(model.as_str())),
        _ => {}
    }
}

fn checked_pointer(pointer: String) -> Result<String, String> {
    if pointer.starts_with('/') && pointer.len() > 1 {
        Ok(pointer)
    } else {
        Err(format!(
            "`{pointer}` is not a JSON Pointer such as /temperature"
        ))
    }
}

fn tokens(pointer: &str) -> impl Iterator<Item = String> + '_ {
    pointer
        .split('/')
        .skip(1)
        .map(|token| token.replace("~1", "/").replace("~0", "~"))
}

/// Sets `pointer` to `value`, creating missing objects on the way. An array index must exist.
fn set_pointer(root: &mut Value, pointer: &str, value: Value) {
    let tokens: Vec<String> = tokens(pointer).collect();
    let Some((last, parents)) = tokens.split_last() else {
        return;
    };
    let mut node = root;
    for token in parents {
        node = match node {
            Value::Object(map) => map
                .entry(token.clone())
                .or_insert_with(|| Value::Object(Map::new())),
            Value::Array(items) => match token.parse::<usize>().ok().and_then(|i| items.get_mut(i))
            {
                Some(item) => item,
                None => return,
            },
            _ => return,
        };
    }
    match node {
        Value::Object(map) => {
            map.insert(last.clone(), value);
        }
        Value::Array(items) => {
            if let Some(item) = last.parse::<usize>().ok().and_then(|i| items.get_mut(i)) {
                *item = value;
            }
        }
        _ => {}
    }
}

/// Removes `pointer` when it exists.
fn remove_pointer(root: &mut Value, pointer: &str) {
    let tokens: Vec<String> = tokens(pointer).collect();
    let Some((last, parents)) = tokens.split_last() else {
        return;
    };
    let mut node = root;
    for token in parents {
        let next = match node {
            Value::Object(map) => map.get_mut(token),
            Value::Array(items) => token.parse::<usize>().ok().and_then(|i| items.get_mut(i)),
            _ => None,
        };
        let Some(next) = next else {
            return;
        };
        node = next;
    }
    match node {
        Value::Object(map) => {
            map.remove(last);
        }
        Value::Array(items) => {
            if let Some(index) = last.parse::<usize>().ok().filter(|i| *i < items.len()) {
                items.remove(index);
            }
        }
        _ => {}
    }
}

/// Replaces each `${NAME}` in `text` with the environment value. An unset name is an error.
pub(crate) fn expand(text: &str, env: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .ok_or_else(|| format!("`{text}` has an unclosed ${{"))?;
        let name = &after[..end];
        let value = env(name).ok_or_else(|| format!("environment variable {name} is not set"))?;
        out.push_str(&value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn expand_value(value: Value, env: &dyn Fn(&str) -> Option<String>) -> Result<Value, String> {
    Ok(match value {
        Value::String(text) => Value::String(expand(&text, env)?),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|v| expand_value(v, env))
                .collect::<Result<_, _>>()?,
        ),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, v)| Ok((key, expand_value(v, env)?)))
                .collect::<Result<_, String>>()?,
        ),
        other => other,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn env(name: &str) -> Option<String> {
        (name == "TEAM").then(|| "blue".to_owned())
    }

    fn action(yaml: &str, phase: PhaseName) -> Result<Action, String> {
        Action::compile(serde_saphyr::from_str(yaml).unwrap(), phase, &env)
    }

    #[test]
    fn header_actions_set_and_remove_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("x-old", HeaderValue::from_static("1"));

        apply_headers(
            &action("set_header: {X-Team: '${TEAM}'}", PhaseName::Attempt).unwrap(),
            &mut headers,
        );
        apply_headers(
            &action("remove_header: [x-old]", PhaseName::Attempt).unwrap(),
            &mut headers,
        );

        assert_eq!(headers["x-team"], "blue");
        assert!(!headers.contains_key("x-old"));
    }

    #[test]
    fn set_fallbacks_writes_the_fallback_header() {
        let mut headers = HeaderMap::new();

        apply_headers(
            &action("set_fallbacks: [a/b, c/d]", PhaseName::Request).unwrap(),
            &mut headers,
        );

        assert_eq!(headers[HEADER_FALLBACKS], "a/b,c/d");
    }

    #[test]
    fn body_actions_set_create_and_remove_fields() {
        let mut body = json!({"model": "x", "temperature": 1, "messages": [{"role": "user"}], "a~b": 1, "c/d": 2});

        for yaml in [
            "set_field: {/metadata/team: '${TEAM}', /messages/0/role: system, /messages/9/role: x, /model/x: 1}",
            "remove_field: [/temperature, /messages/0/missing, /none/x, /a~0b, /c~1d]",
            "set_model: groq/llama",
        ] {
            let action = action(yaml, PhaseName::Request).unwrap();
            assert!(action.changes_body());
            apply_body(&action, &mut body);
        }

        assert_eq!(
            body,
            json!({"model": "groq/llama", "messages": [{"role": "system"}], "metadata": {"team": "blue"}})
        );
    }

    #[test]
    fn array_items_can_be_removed_and_scalars_are_left_alone() {
        let mut body = json!({"tools": [1, 2, 3], "n": 1});

        apply_body(
            &action(
                "remove_field: [/tools/1, /tools/7, /n/x]",
                PhaseName::Attempt,
            )
            .unwrap(),
            &mut body,
        );
        apply_body(
            &action("set_field: {/n/x: 1, /tools/0: 9}", PhaseName::Attempt).unwrap(),
            &mut body,
        );

        assert_eq!(body, json!({"tools": [9, 3], "n": 1}));
    }

    #[test]
    fn reject_retry_and_status_compile_to_their_actions() {
        assert!(matches!(
            action("reject: {status: 403, message: 'no ${TEAM}'}", PhaseName::Request).unwrap(),
            Action::Reject(StatusCode::FORBIDDEN, ref m) if m == "no blue"
        ));
        assert!(matches!(
            action("retry_next: true", PhaseName::Response).unwrap(),
            Action::RetryNext
        ));
        assert!(matches!(
            action("set_status: 429", PhaseName::Response).unwrap(),
            Action::SetStatus(StatusCode::TOO_MANY_REQUESTS)
        ));
        assert!(
            !action("retry_next: true", PhaseName::Response)
                .unwrap()
                .changes_body()
        );
    }

    #[test]
    fn an_action_the_phase_cannot_run_is_an_error() {
        for (yaml, phase) in [
            ("retry_next: true", PhaseName::Request),
            ("set_fallbacks: [a/b]", PhaseName::Attempt),
            ("reject: {status: 400, message: x}", PhaseName::Response),
            ("set_model: x", PhaseName::Response),
            ("set_status: 500", PhaseName::Attempt),
        ] {
            let error = action(yaml, phase).unwrap_err();
            assert!(error.contains("not available"), "{error}");
        }
    }

    #[test]
    fn invalid_values_are_errors() {
        for (yaml, phase) in [
            ("set_header: {'bad name': x}", PhaseName::Request),
            ("set_header: {x-a: '${MISSING}'}", PhaseName::Request),
            ("set_field: {temperature: 1}", PhaseName::Request),
            ("remove_field: /", PhaseName::Request),
            ("reject: {status: 200, message: x}", PhaseName::Request),
            ("retry_next: false", PhaseName::Response),
            ("set_status: 42", PhaseName::Response),
            ("set_model: '${OPEN'", PhaseName::Request),
            ("set_fallbacks: ['${MISSING}']", PhaseName::Request),
        ] {
            assert!(action(yaml, phase).is_err(), "{yaml}");
        }
    }

    #[test]
    fn expand_replaces_every_variable() {
        assert_eq!(expand("a-${TEAM}-${TEAM}", &env).unwrap(), "a-blue-blue");
        assert_eq!(expand("plain", &env).unwrap(), "plain");
        assert_eq!(
            expand_value(json!({"k": ["${TEAM}", 1, true]}), &env).unwrap(),
            json!({"k": ["blue", 1, true]})
        );
    }

    #[test]
    fn header_actions_ignore_body_actions_and_the_reverse() {
        let mut headers = HeaderMap::new();
        let mut body = json!({"a": 1});

        apply_headers(
            &action("remove_field: /a", PhaseName::Request).unwrap(),
            &mut headers,
        );
        apply_body(
            &action("set_header: {x-a: '1'}", PhaseName::Request).unwrap(),
            &mut body,
        );

        assert!(headers.is_empty());
        assert_eq!(body, json!({"a": 1}));
    }
}
