/// The client API a request uses, read from its path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Api {
    /// `/v1/chat/completions`
    Chat,
    /// `/v1/messages`
    Messages,
    /// `/v1/responses`
    Responses,
    /// Any other `/v1/*` path, such as embeddings or audio.
    Other,
}

impl Api {
    /// The API of `path`, with or without a query string.
    #[must_use]
    pub fn from_path(path: &str) -> Api {
        let bare = path.split('?').next().unwrap_or(path);
        match bare.trim_start_matches("/v1") {
            "/chat/completions" => Api::Chat,
            "/messages" => Api::Messages,
            "/responses" => Api::Responses,
            _ => Api::Other,
        }
    }

    /// The name rules use for this API.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Api::Chat => "chat",
            Api::Messages => "messages",
            Api::Responses => "responses",
            Api::Other => "other",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_client_path_maps_to_its_api() {
        for (path, api) in [
            ("/v1/chat/completions", Api::Chat),
            ("/chat/completions", Api::Chat),
            ("/v1/messages", Api::Messages),
            ("/v1/responses?stream=true", Api::Responses),
            ("/v1/embeddings", Api::Other),
            ("/v1/messages/count_tokens", Api::Other),
        ] {
            assert_eq!(Api::from_path(path), api, "{path}");
        }
    }

    #[test]
    fn each_api_has_its_rule_name() {
        let names: Vec<&str> = [Api::Chat, Api::Messages, Api::Responses, Api::Other]
            .into_iter()
            .map(Api::as_str)
            .collect();
        assert_eq!(names, ["chat", "messages", "responses", "other"]);
    }
}
