use axum::http::HeaderMap;
use serde_json::Value;

/// The profiles a model list is limited to. Empty means every profile.
/// Agent commands take it as `--profiles a,b`, and the hub reads it from the
/// `x-llm-hub-profiles` header or the `?profiles=` query.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProfileFilter {
    names: Vec<String>,
}

impl ProfileFilter {
    pub const HEADER: &str = "x-llm-hub-profiles";
    pub const QUERY_PARAM: &str = "profiles";
    const FLAG: &str = "--profiles";
    const END_OF_OPTIONS: &str = "--";

    /// Reads a comma-separated list leniently: names are trimmed and
    /// lowercased, and empty entries are dropped.
    #[must_use]
    pub fn from_list(list: &str) -> Self {
        let mut filter = Self::default();
        for name in list
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            filter.add(name.to_lowercase());
        }
        filter
    }

    /// The filter a hub request asks for: `?profiles=` wins over the header.
    #[must_use]
    pub fn from_request(query: Option<&str>, headers: &HeaderMap) -> Self {
        let from_query = query.map(Self::from_list).unwrap_or_default();
        if !from_query.is_empty() {
            return from_query;
        }
        headers
            .get(Self::HEADER)
            .and_then(|value| value.to_str().ok())
            .map(Self::from_list)
            .unwrap_or_default()
    }

    /// Removes every `--profiles a,b` and `--profiles=a,b` at the front of `args`.
    ///
    /// # Errors
    /// Returns a message when a flag has no value or names an invalid profile.
    pub fn take_leading_flags(args: &mut Vec<String>) -> Result<Self, String> {
        let mut filter = Self::default();
        while let Some(value) = Self::take_flag_at(args, 0)? {
            filter.merge(Self::parse_flag_value(&value)?);
        }
        Ok(filter)
    }

    /// Removes every `--profiles a,b` and `--profiles=a,b` before a `--`.
    ///
    /// # Errors
    /// Returns a message when a flag has no value or names an invalid profile.
    pub fn take_flags(args: &mut Vec<String>) -> Result<Self, String> {
        let mut filter = Self::default();
        let mut index = 0;
        while index < args.len() && args[index] != Self::END_OF_OPTIONS {
            match Self::take_flag_at(args, index)? {
                Some(value) => filter.merge(Self::parse_flag_value(&value)?),
                None => index += 1,
            }
        }
        Ok(filter)
    }

    /// True when every profile is allowed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    /// True when the qualified `<profile>/<model>` id belongs to an allowed profile.
    #[must_use]
    pub fn allows(&self, id: &str) -> bool {
        self.is_empty()
            || id
                .split_once('/')
                .is_some_and(|(profile, _)| self.names.iter().any(|name| name == profile))
    }

    /// Keeps only the allowed ids in a `{data: [...]}` model list.
    pub fn retain_models(&self, payload: &mut Value) {
        if self.is_empty() {
            return;
        }
        if let Some(models) = payload.get_mut("data").and_then(Value::as_array_mut) {
            models.retain(|model| {
                model
                    .get("id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| self.allows(id))
            });
        }
    }

    /// The names joined with commas, for the header and the query.
    #[must_use]
    pub fn to_list(&self) -> String {
        self.names.join(",")
    }

    /// Adds the other filter's names, keeping the first-seen order.
    pub fn merge(&mut self, other: Self) {
        for name in other.names {
            self.add(name);
        }
    }

    fn add(&mut self, name: String) {
        if !self.names.contains(&name) {
            self.names.push(name);
        }
    }

    /// A flag value must name at least one profile, and every name must be
    /// a valid profile name, so it is safe inside a header line.
    fn parse_flag_value(value: &str) -> Result<Self, String> {
        let filter = Self::from_list(value);
        if filter.is_empty() {
            return Err(format!("{} needs at least one profile name", Self::FLAG));
        }
        if let Some(bad) = filter.names.iter().find(|name| !Self::is_valid_name(name)) {
            return Err(format!("{}: invalid profile name '{bad}'", Self::FLAG));
        }
        Ok(filter)
    }

    fn is_valid_name(name: &str) -> bool {
        name.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    }

    /// The value of the flag at `args[index]`, removed from `args`, if one is there.
    fn take_flag_at(args: &mut Vec<String>, index: usize) -> Result<Option<String>, String> {
        let Some(arg) = args.get(index) else {
            return Ok(None);
        };
        if let Some(value) = arg
            .strip_prefix(Self::FLAG)
            .and_then(|rest| rest.strip_prefix('='))
        {
            let value = value.to_string();
            args.remove(index);
            return Ok(Some(value));
        }
        if arg != Self::FLAG {
            return Ok(None);
        }
        let value = args
            .get(index + 1)
            .filter(|value| !value.starts_with('-'))
            .cloned()
            .ok_or_else(|| format!("{} needs a value, e.g. {} copilot", Self::FLAG, Self::FLAG))?;
        args.drain(index..=index + 1);
        Ok(Some(value))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn list_is_trimmed_lowercased_and_deduplicated() {
        let filter = ProfileFilter::from_list(" Copilot, ivrit,,copilot ");

        assert_eq!(filter.to_list(), "copilot,ivrit");
    }

    #[test]
    fn allows_only_ids_under_a_listed_profile() {
        let filter = ProfileFilter::from_list("copilot,ivrit");

        assert!(filter.allows("copilot/auto"));
        assert!(filter.allows("ivrit/model-a"));
        assert!(!filter.allows("local/qwen"));
        assert!(!filter.allows("copilot"));
        assert!(!filter.allows("copilotx/auto"));
    }

    #[test]
    fn empty_filter_allows_everything_and_keeps_the_list() {
        let mut payload = json!({"data": [{"id": "a/x"}, {"id": "b/y"}]});

        ProfileFilter::default().retain_models(&mut payload);

        assert_eq!(payload, json!({"data": [{"id": "a/x"}, {"id": "b/y"}]}));
    }

    #[test]
    fn retain_keeps_listed_profiles_in_order() {
        let mut payload = json!({"object": "list", "data": [
            {"id": "ivrit/m"}, {"id": "local/q"}, {"id": "copilot/auto"}, {"no_id": true},
        ]});

        ProfileFilter::from_list("copilot,ivrit").retain_models(&mut payload);

        assert_eq!(
            payload,
            json!({"object": "list", "data": [{"id": "ivrit/m"}, {"id": "copilot/auto"}]})
        );
    }

    #[test]
    fn query_wins_over_the_header() {
        let mut headers = HeaderMap::new();
        headers.insert(ProfileFilter::HEADER, "local".parse().unwrap());

        assert_eq!(
            ProfileFilter::from_request(Some("copilot"), &headers).to_list(),
            "copilot"
        );
        assert_eq!(
            ProfileFilter::from_request(Some(""), &headers).to_list(),
            "local"
        );
        assert!(ProfileFilter::from_request(None, &HeaderMap::new()).is_empty());
    }

    #[test]
    fn leading_flags_are_taken_in_both_forms() {
        let mut args = strings(&["--profiles", "copilot", "--profiles=ivrit", "claude", "m"]);

        let filter = ProfileFilter::take_leading_flags(&mut args).unwrap();

        assert_eq!(filter.to_list(), "copilot,ivrit");
        assert_eq!(args, ["claude", "m"]);
    }

    #[test]
    fn leading_flags_stop_at_the_first_other_argument() {
        let mut args = strings(&["claude", "--profiles", "copilot"]);

        let filter = ProfileFilter::take_leading_flags(&mut args).unwrap();

        assert!(filter.is_empty());
        assert_eq!(args, ["claude", "--profiles", "copilot"]);
    }

    #[test]
    fn flags_are_taken_anywhere_before_end_of_options() {
        let mut args = strings(&[
            "m",
            "--profiles=copilot",
            "-p",
            "--profiles",
            "ivrit",
            "--",
            "--profiles=x",
        ]);

        let filter = ProfileFilter::take_flags(&mut args).unwrap();

        assert_eq!(filter.to_list(), "copilot,ivrit");
        assert_eq!(args, ["m", "-p", "--", "--profiles=x"]);
    }

    #[test]
    fn missing_flag_value_is_an_error() {
        for args in [&["--profiles"][..], &["--profiles", "-p"], &["--profiles="]] {
            assert!(ProfileFilter::take_flags(&mut strings(args)).is_err());
        }
    }

    #[test]
    fn invalid_profile_name_is_an_error() {
        let error = ProfileFilter::take_flags(&mut strings(&["--profiles=a\nb: c"])).unwrap_err();

        assert!(error.contains("invalid profile name"), "{error}");
    }
}
