//! The `local-worker` mode: serves one `mistralrs` model on a local port.

#[cfg(feature = "mistralrs")]
mod engine;
#[cfg_attr(
    not(feature = "mistralrs"),
    allow(dead_code, reason = "only the mistralrs engine fills request bodies")
)]
mod request_defaults;

use std::path::PathBuf;

use super::mistralrs_flags::{self, MistralrsOptions};

/// The error of a worker started from a binary without the `mistralrs` feature.
pub const MISTRALRS_NOT_BUILT: &str =
    "this llm-hub was built without the `mistralrs` cargo feature";

/// The arguments of one `local-worker` run, as `command.rs` writes them.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkerArgs {
    /// Registry id of the model, sent to clients as the model name.
    pub id: String,
    /// Path of the GGUF model file.
    pub model: PathBuf,
    /// Loopback port to listen on.
    pub port: u16,
    /// Sampling defaults, context length, thinking switch and vision files.
    pub options: MistralrsOptions,
}

/// Runs the worker with the words after `local-worker`.
///
/// # Errors
///
/// Returns a message when an argument is missing or invalid, when the binary lacks the
/// `mistralrs` feature, or when the model cannot load or the port cannot bind.
pub async fn run(words: &[String]) -> Result<(), String> {
    let args = parse_args(words)?;
    serve(args).await
}

#[cfg(feature = "mistralrs")]
async fn serve(args: WorkerArgs) -> Result<(), String> {
    engine::serve(args).await
}

#[cfg(not(feature = "mistralrs"))]
#[allow(
    clippy::unused_async,
    reason = "matches the feature build, where serving is async"
)]
async fn serve(_args: WorkerArgs) -> Result<(), String> {
    Err(MISTRALRS_NOT_BUILT.to_owned())
}

/// Splits the worker words into the identity flags and the flags of [`mistralrs_flags::parse`].
///
/// # Errors
///
/// Returns a message when `--id`, `--model` or `--port` is missing, lacks a value, or the port
/// is not a number, or when `--ctx-size` is missing or zero.
pub fn parse_args(words: &[String]) -> Result<WorkerArgs, String> {
    let mut id = None;
    let mut model = None;
    let mut port = None;
    let mut option_words = Vec::new();
    let mut iter = words.iter();
    while let Some(word) = iter.next() {
        let slot = match word.as_str() {
            "--id" => &mut id,
            "--model" => &mut model,
            "--port" => &mut port,
            _ => {
                option_words.push(word.clone());
                continue;
            }
        };
        let value = iter
            .next()
            .ok_or_else(|| format!("local-worker: {word} needs a value"))?;
        *slot = Some(value.clone());
    }
    let id = id.ok_or("local-worker: --id is required")?;
    let model = model.ok_or("local-worker: --model is required")?;
    let port_text = port.ok_or("local-worker: --port is required")?;
    let port = port_text
        .parse::<u16>()
        .map_err(|_| format!("local-worker: --port {port_text:?} is not a port number"))?;
    let options = mistralrs_flags::parse(&id, 0, &option_words)?;
    if options.ctx_size == 0 {
        return Err("local-worker: --ctx-size is required and must be above 0".to_owned());
    }
    Ok(WorkerArgs {
        id,
        model: PathBuf::from(model),
        port,
        options,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn a_full_command_line_parses_into_the_worker_args() {
        let args = parse_args(&words(
            "--id m --model /models/m.gguf --port 5801 --ctx-size 4096 --temp 0.6 \
             --reasoning off --mmproj /p.gguf --tok-model-id org/x",
        ))
        .unwrap();
        assert_eq!(args.id, "m");
        assert_eq!(args.model, PathBuf::from("/models/m.gguf"));
        assert_eq!(args.port, 5801);
        assert_eq!(args.options.ctx_size, 4096);
        assert!((args.options.temp - 0.6).abs() < 1e-9);
        assert_eq!(args.options.reasoning, Some(false));
        assert_eq!(args.options.tok_model_id.as_deref(), Some("org/x"));
    }

    #[test]
    fn a_missing_identity_flag_is_an_error() {
        let err = parse_args(&words("--model m --port 1 --ctx-size 8")).unwrap_err();
        assert_eq!(err, "local-worker: --id is required");
        let err = parse_args(&words("--id m --port 1 --ctx-size 8")).unwrap_err();
        assert_eq!(err, "local-worker: --model is required");
        let err = parse_args(&words("--id m --model m --ctx-size 8")).unwrap_err();
        assert_eq!(err, "local-worker: --port is required");
    }

    #[test]
    fn a_flag_without_its_value_is_an_error() {
        let err = parse_args(&words("--id")).unwrap_err();
        assert_eq!(err, "local-worker: --id needs a value");
    }

    #[test]
    fn a_port_that_is_not_a_number_is_an_error() {
        let err = parse_args(&words("--id m --model m --port http --ctx-size 8")).unwrap_err();
        assert_eq!(err, "local-worker: --port \"http\" is not a port number");
    }

    #[test]
    fn a_zero_or_missing_context_is_an_error() {
        let err = parse_args(&words("--id m --model m --port 1")).unwrap_err();
        assert_eq!(
            err,
            "local-worker: --ctx-size is required and must be above 0"
        );
        let err = parse_args(&words("--id m --model m --port 1 --ctx-size 0")).unwrap_err();
        assert_eq!(
            err,
            "local-worker: --ctx-size is required and must be above 0"
        );
    }

    #[test]
    fn an_unsupported_flag_is_named_by_the_flag_parser() {
        let err = parse_args(&words("--id m --model m --port 1 --ctx-size 8 --jinja")).unwrap_err();
        assert!(err.contains("--jinja is not supported"), "{err}");
    }

    #[cfg(not(feature = "mistralrs"))]
    #[tokio::test]
    async fn run_without_the_feature_names_it() {
        let err = run(&words("--id m --model m --port 1 --ctx-size 8"))
            .await
            .unwrap_err();
        assert_eq!(err, MISTRALRS_NOT_BUILT);
    }

    #[cfg(not(feature = "mistralrs"))]
    #[tokio::test]
    async fn run_checks_the_arguments_before_the_feature() {
        let err = run(&words("--id m")).await.unwrap_err();
        assert_eq!(err, "local-worker: --model is required");
    }
}
