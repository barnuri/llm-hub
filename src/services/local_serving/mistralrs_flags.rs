//! Maps the `extra_flags` of a `mistralrs` registry row to the options of the `local-worker`.

use std::path::PathBuf;
use std::str::FromStr;

/// Sampling defaults that `cmd_sync_swap` gives every llama row. The row's flags win over them.
const DEFAULT_TEMP: f64 = 1.0;
const DEFAULT_TOP_P: f64 = 0.95;
const DEFAULT_TOP_K: u64 = 40;
const DEFAULT_MIN_P: f64 = 0.01;

/// Options of one `mistralrs` row, after the shared defaults and the row's flags.
#[derive(Debug, Clone, PartialEq)]
pub struct MistralrsOptions {
    /// Maximum sequence length: `--ctx-size` or `-c`, else the row's ctx column.
    pub ctx_size: u64,
    /// Sampling temperature: `--temp`.
    pub temp: f64,
    /// Nucleus sampling: `--top-p`.
    pub top_p: f64,
    /// Top-k sampling: `--top-k`.
    pub top_k: u64,
    /// Min-p sampling: `--min-p`.
    pub min_p: f64,
    /// Thinking switch: `--reasoning on` is `Some(true)`, `off` is `Some(false)`, unset is `None`.
    pub reasoning: Option<bool>,
    /// Multimodal projector file: `--mmproj`, needed by vision models such as gemma 4.
    pub mmproj: Option<PathBuf>,
    /// Hugging Face id of the tokenizer: `--tok-model-id`, needed by gemma 4.
    pub tok_model_id: Option<String>,
}

impl MistralrsOptions {
    fn with_defaults(ctx_size: u64) -> Self {
        Self {
            ctx_size,
            temp: DEFAULT_TEMP,
            top_p: DEFAULT_TOP_P,
            top_k: DEFAULT_TOP_K,
            min_p: DEFAULT_MIN_P,
            reasoning: None,
            mmproj: None,
            tok_model_id: None,
        }
    }

    /// The worker's command-line words for these options, `--ctx-size` included.
    pub fn worker_args(&self) -> Vec<String> {
        let mut args = vec![
            "--ctx-size".to_owned(),
            self.ctx_size.to_string(),
            "--temp".to_owned(),
            format!("{:?}", self.temp),
            "--top-p".to_owned(),
            format!("{:?}", self.top_p),
            "--top-k".to_owned(),
            self.top_k.to_string(),
            "--min-p".to_owned(),
            format!("{:?}", self.min_p),
        ];
        if let Some(on) = self.reasoning {
            args.push("--reasoning".to_owned());
            args.push(if on { "on" } else { "off" }.to_owned());
        }
        if let Some(path) = &self.mmproj {
            args.push("--mmproj".to_owned());
            args.push(path.to_string_lossy().into_owned());
        }
        if let Some(id) = &self.tok_model_id {
            args.push("--tok-model-id".to_owned());
            args.push(id.clone());
        }
        args
    }
}

/// Maps `words` (the row's `extra_flags`, already split) onto the options.
///
/// Accepts `flag value` and `flag=value`. A flag outside the supported set is an error, so a
/// llama-only flag cannot be dropped silently.
///
/// # Errors
///
/// Returns a message naming the row and the flag when a flag is not supported, lacks its value,
/// or has a value that does not parse.
pub fn parse(row_id: &str, row_ctx: u64, words: &[String]) -> Result<MistralrsOptions, String> {
    let mut opts = MistralrsOptions::with_defaults(row_ctx);
    let mut iter = words.iter();
    while let Some(word) = iter.next() {
        let (flag, inline) = split_inline(word);
        let mut value = || match &inline {
            Some(value) => Ok(value.clone()),
            None => iter
                .next()
                .cloned()
                .ok_or_else(|| format!("model {row_id}: extra_flags {flag} needs a value")),
        };
        match flag {
            "--ctx-size" | "-c" => opts.ctx_size = number(row_id, flag, &value()?)?,
            "--temp" => opts.temp = number(row_id, flag, &value()?)?,
            "--top-p" => opts.top_p = number(row_id, flag, &value()?)?,
            "--top-k" => opts.top_k = number(row_id, flag, &value()?)?,
            "--min-p" => opts.min_p = number(row_id, flag, &value()?)?,
            "--reasoning" => {
                opts.reasoning = match value()?.as_str() {
                    "on" => Some(true),
                    "off" => Some(false),
                    other => {
                        return Err(format!(
                            "model {row_id}: extra_flags --reasoning takes on or off: {other}"
                        ));
                    }
                };
            }
            "--mmproj" => opts.mmproj = Some(PathBuf::from(value()?)),
            "--tok-model-id" => opts.tok_model_id = Some(value()?),
            _ => {
                return Err(format!(
                    "model {row_id}: extra_flags {flag} is not supported by the mistralrs engine"
                ));
            }
        }
    }
    Ok(opts)
}

/// Splits `--flag=value` into its flag and value. A word without `=` returns no value.
fn split_inline(word: &str) -> (&str, Option<String>) {
    match word.split_once('=') {
        Some((flag, value)) if word.starts_with('-') => (flag, Some(value.to_owned())),
        _ => (word, None),
    }
}

fn number<T: FromStr>(row_id: &str, flag: &str, raw: &str) -> Result<T, String> {
    raw.parse::<T>()
        .map_err(|_| format!("model {row_id}: extra_flags {flag} value {raw:?} is not a number"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn no_flags_give_the_shared_sampling_defaults_and_the_row_ctx() {
        let opts = parse("m", 8192, &[]).unwrap();
        assert_eq!(opts, MistralrsOptions::with_defaults(8192));
        assert!((opts.temp - 1.0).abs() < 1e-9);
        assert!((opts.top_p - 0.95).abs() < 1e-9);
        assert_eq!(opts.top_k, 40);
        assert!((opts.min_p - 0.01).abs() < 1e-9);
    }

    #[test]
    fn row_flags_override_the_defaults() {
        let opts = parse(
            "m",
            8192,
            &words("--ctx-size 4096 --temp 0.6 --top-p=0.9 -c 2048 --top-k 20 --min-p 0.05"),
        )
        .unwrap();
        assert_eq!(opts.ctx_size, 2048);
        assert!((opts.temp - 0.6).abs() < 1e-9);
        assert!((opts.top_p - 0.9).abs() < 1e-9);
        assert_eq!(opts.top_k, 20);
        assert!((opts.min_p - 0.05).abs() < 1e-9);
    }

    #[test]
    fn reasoning_switch_maps_on_and_off() {
        assert_eq!(
            parse("m", 1, &words("--reasoning off")).unwrap().reasoning,
            Some(false)
        );
        assert_eq!(
            parse("m", 1, &words("--reasoning=on")).unwrap().reasoning,
            Some(true)
        );
    }

    #[test]
    fn reasoning_with_another_value_is_an_error() {
        let err = parse("m", 1, &words("--reasoning maybe")).unwrap_err();
        assert_eq!(
            err,
            "model m: extra_flags --reasoning takes on or off: maybe"
        );
    }

    #[test]
    fn vision_flags_are_kept_as_given() {
        let opts = parse(
            "gemma",
            1,
            &words("--mmproj /models/mmproj.gguf --tok-model-id google/gemma-4-E4B-it"),
        )
        .unwrap();
        assert_eq!(opts.mmproj, Some(PathBuf::from("/models/mmproj.gguf")));
        assert_eq!(opts.tok_model_id.as_deref(), Some("google/gemma-4-E4B-it"));
    }

    #[test]
    fn an_unsupported_flag_names_the_flag_and_the_row() {
        let err = parse("m", 1, &words("--spec-type ngram-cache")).unwrap_err();
        assert_eq!(
            err,
            "model m: extra_flags --spec-type is not supported by the mistralrs engine"
        );
        let err = parse("m", 1, &words("--jinja")).unwrap_err();
        assert!(err.contains("--jinja is not supported"), "{err}");
    }

    #[test]
    fn a_flag_without_a_value_is_an_error() {
        let err = parse("m", 1, &words("--temp")).unwrap_err();
        assert_eq!(err, "model m: extra_flags --temp needs a value");
    }

    #[test]
    fn a_non_numeric_value_is_an_error() {
        let err = parse("m", 1, &words("--top-k many")).unwrap_err();
        assert_eq!(
            err,
            "model m: extra_flags --top-k value \"many\" is not a number"
        );
    }

    #[test]
    fn worker_args_carry_every_option_and_skip_unset_ones() {
        let plain = parse("m", 4096, &[]).unwrap().worker_args();
        assert_eq!(
            plain,
            words("--ctx-size 4096 --temp 1.0 --top-p 0.95 --top-k 40 --min-p 0.01")
        );
        let full = parse(
            "gemma",
            4096,
            &words("--reasoning off --mmproj /m.gguf --tok-model-id org/x"),
        )
        .unwrap()
        .worker_args();
        assert_eq!(
            full,
            words(
                "--ctx-size 4096 --temp 1.0 --top-p 0.95 --top-k 40 --min-p 0.01 \
                 --reasoning off --mmproj /m.gguf --tok-model-id org/x"
            )
        );
    }
}
