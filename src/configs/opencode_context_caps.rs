use std::collections::BTreeMap;

use crate::utils::cli_flag::take_flag_value;

/// Per-model caps on the context window `llm-hub opencode` writes into its provider config.
/// `LLM_HUB_OPENCODE_CONTEXT_CAPS` and the repeatable `--context-cap` flag hold comma-separated
/// `<model id>=<tokens>` pairs. Model ids match exactly, and a later pair for a model wins.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpencodeContextCaps {
    caps: BTreeMap<String, u64>,
}

impl OpencodeContextCaps {
    const KEY: &str = "LLM_HUB_OPENCODE_CONTEXT_CAPS";
    const FLAG: &str = "--context-cap";
    const END_OF_OPTIONS: &str = "--";
    /// A clamped output limit is this fraction of the capped context.
    const OUTPUT_DIVISOR: u64 = 4;

    /// Reads `LLM_HUB_OPENCODE_CONTEXT_CAPS`. An unset or empty value means no caps.
    ///
    /// # Errors
    /// Returns a message naming the variable for an entry without `=`, a model id that is empty,
    /// or a token count that is not a positive integer.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        lookup(Self::KEY).map_or_else(|| Ok(Self::default()), |raw| Self::parse(&raw, Self::KEY))
    }

    /// Removes every `--context-cap <model>=<tokens>` and `--context-cap=<model>=<tokens>` before
    /// a `--`. A value may also hold several comma-separated pairs.
    ///
    /// # Errors
    /// Returns a message for a flag without a value or with an invalid pair.
    pub fn take_flags(args: &mut Vec<String>) -> Result<Self, String> {
        let mut caps = Self::default();
        let mut index = 0;
        while index < args.len() && args[index] != Self::END_OF_OPTIONS {
            match take_flag_value(args, index, Self::FLAG, "copilot/model=200000")? {
                Some(value) => caps.override_with(Self::parse(&value, Self::FLAG)?),
                None => index += 1,
            }
        }
        Ok(caps)
    }

    /// Replaces this set's cap for every model `other` names.
    pub fn override_with(&mut self, other: Self) {
        self.caps.extend(other.caps);
    }

    /// The `(context, output)` limits for `model`: the context is at most the model's cap. An
    /// output limit above the capped context becomes a quarter of it, rounded down.
    #[must_use]
    pub fn limits(&self, model: &str, context: u64, output: u64) -> (u64, u64) {
        let Some(cap) = self.caps.get(model) else {
            return (context, output);
        };
        let context = context.min(*cap);
        if output > context {
            return (context, context / Self::OUTPUT_DIVISOR);
        }
        (context, output)
    }

    fn parse(raw: &str, source: &str) -> Result<Self, String> {
        let mut caps = Self::default();
        for entry in raw
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
        {
            let (model, tokens) = entry
                .rsplit_once('=')
                .ok_or_else(|| format!("{source} entry {entry:?} is not <model id>=<tokens>"))?;
            let model = model.trim();
            if model.is_empty() {
                return Err(format!("{source} entry {entry:?} has no model id"));
            }
            let tokens = tokens
                .trim()
                .parse::<u64>()
                .ok()
                .filter(|tokens| *tokens > 0)
                .ok_or_else(|| {
                    format!("{source} entry {entry:?} needs a positive whole number of tokens")
                })?;
            caps.caps.insert(model.to_string(), tokens);
        }
        Ok(caps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(raw: &str) -> Result<OpencodeContextCaps, String> {
        OpencodeContextCaps::from_lookup(|name| {
            (name == "LLM_HUB_OPENCODE_CONTEXT_CAPS").then(|| raw.to_string())
        })
    }

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn valid_pairs_are_read_and_trimmed() {
        let caps = parsed(" copilot/claude-opus-5-5[1m] = 200000 ,, local/q=8000").unwrap();

        assert_eq!(
            caps.limits("copilot/claude-opus-5-5[1m]", 1_000_000, 32_000),
            (200_000, 32_000)
        );
        assert_eq!(caps.limits("local/q", 65_536, 16_384), (8000, 2000));
    }

    #[test]
    fn unset_or_empty_means_no_caps() {
        assert_eq!(
            OpencodeContextCaps::from_lookup(|_| None),
            Ok(OpencodeContextCaps::default())
        );
        assert_eq!(parsed(""), Ok(OpencodeContextCaps::default()));
    }

    #[test]
    fn a_later_pair_for_the_same_model_wins() {
        let caps = parsed("m=100000,m=50000").unwrap();

        assert_eq!(caps.limits("m", 200_000, 8000), (50_000, 8000));
    }

    #[test]
    fn invalid_entries_fail_with_the_variable_name() {
        assert_eq!(
            parsed("m"),
            Err(
                r#"LLM_HUB_OPENCODE_CONTEXT_CAPS entry "m" is not <model id>=<tokens>"#.to_string()
            )
        );
        assert_eq!(
            parsed("=5"),
            Err(r#"LLM_HUB_OPENCODE_CONTEXT_CAPS entry "=5" has no model id"#.to_string())
        );
        for bad in ["m=abc", "m=0", "m=-5", "m="] {
            let error = parsed(bad).unwrap_err();
            assert!(error.contains("positive whole number"), "{bad}: {error}");
        }
    }

    #[test]
    fn a_model_without_a_cap_keeps_its_limits() {
        let caps = parsed("other=1000").unwrap();

        assert_eq!(caps.limits("m", 1_000_000, 32_768), (1_000_000, 32_768));
    }

    #[test]
    fn a_cap_above_the_window_keeps_the_window() {
        let caps = parsed("m=2000000").unwrap();

        assert_eq!(caps.limits("m", 128_000, 32_768), (128_000, 32_768));
    }

    #[test]
    fn output_above_the_capped_context_is_a_quarter_of_it() {
        let caps = parsed("m=100001").unwrap();

        assert_eq!(caps.limits("m", 1_000_000, 128_000), (100_001, 25_000));
        assert_eq!(caps.limits("m", 1_000_000, 100_001), (100_001, 100_001));
    }

    #[test]
    fn flags_are_taken_before_end_of_options_and_later_ones_win() {
        let mut args = strings(&[
            "--context-cap",
            "a=10",
            "-c",
            "--context-cap=a=20,b=30",
            "--",
            "--context-cap=c=1",
        ]);

        let caps = OpencodeContextCaps::take_flags(&mut args).unwrap();

        assert_eq!(args, ["-c", "--", "--context-cap=c=1"]);
        assert_eq!(caps.limits("a", 100, 1), (20, 1));
        assert_eq!(caps.limits("b", 100, 1), (30, 1));
        assert_eq!(caps.limits("c", 100, 1), (100, 1));
    }

    #[test]
    fn bad_flags_fail_with_the_flag_name() {
        assert_eq!(
            OpencodeContextCaps::take_flags(&mut strings(&["--context-cap"])),
            Err("--context-cap needs a value, e.g. --context-cap copilot/model=200000".to_string())
        );
        let error =
            OpencodeContextCaps::take_flags(&mut strings(&["--context-cap=m=0"])).unwrap_err();
        assert!(error.starts_with("--context-cap entry"), "{error}");
    }

    #[test]
    fn flag_caps_override_the_setting_for_the_same_model() {
        let mut caps = parsed("a=100,b=200").unwrap();

        caps.override_with(parsed("a=50").unwrap());

        assert_eq!(caps.limits("a", 1000, 1), (50, 1));
        assert_eq!(caps.limits("b", 1000, 1), (200, 1));
    }
}
