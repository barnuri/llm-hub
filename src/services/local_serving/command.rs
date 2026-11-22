//! Builds the command line that starts a local model process.

use std::path::Path;

use super::check_gate;
use super::mistralrs_flags::{self, MistralrsOptions};
use super::registry_rows::{Engine, RegistryRow};
use super::worker::MISTRALRS_NOT_BUILT;
use crate::configs::{LocalServingConfig, ManagedCommand};

/// The `llama` wrapper's subcommand that runs `llama-server`.
const LLAMA_SERVE: &str = "serve";
/// The shared full Metal offload, skipped when a row sets its own GPU layer count.
const SHARED_GPU_LAYERS: [&str; 2] = ["--gpu-layers", "99"];
/// Flags of `llama-server` that set the GPU layer count.
const GPU_LAYER_FLAGS: [&str; 3] = ["--gpu-layers", "--n-gpu-layers", "-ngl"];
/// Shared tuning that `cmd_sync_swap` in `llama-models` puts before `--gpu-layers`.
const SHARED_BEFORE_GPU: [&str; 10] = [
    "--parallel",
    "1",
    "--batch-size",
    "4096",
    "--ubatch-size",
    "2048",
    "--cache-reuse",
    "256",
    "--flash-attn",
    "on",
];
/// Shared tuning that `cmd_sync_swap` puts after `--gpu-layers`, before the row's flags.
const SHARED_AFTER_GPU: [&str; 11] = [
    "--metrics",
    "--temp",
    "1.0",
    "--top-p",
    "0.95",
    "--top-k",
    "40",
    "--min-p",
    "0.01",
    "--repeat-penalty",
    "1.0",
];
/// True when this binary embeds the mistral.rs worker (cargo feature `mistralrs`).
const MISTRALRS_BUILT: bool = cfg!(feature = "mistralrs");
/// The worker subcommand of this binary that serves a `mistralrs` row.
const WORKER_SUBCOMMAND: &str = "local-worker";

/// Builds the argv (program first, no shell) that serves `row` from `model_path` on `port`.
/// `home` expands a leading `~` in `bin_override` and in `extra_flags` words.
///
/// # Errors
///
/// Returns a message when `extra_flags` has unbalanced quotes or an unsupported flag, when the
/// binary or model path is not valid UTF-8, when a `mistralrs` row has no passed gate, or when
/// this binary lacks the `mistralrs` feature.
pub fn build_argv(
    row: &RegistryRow,
    config: &LocalServingConfig,
    model_path: &Path,
    port: u16,
    home: Option<&str>,
) -> Result<Vec<String>, String> {
    match row.engine {
        Engine::Llama => llama_argv(row, &config.llama_bin, model_path, port, home),
        Engine::Mistralrs => {
            let exe = std::env::current_exe().map_err(|err| {
                format!("model {}: cannot find the llm-hub binary: {err}", row.id)
            })?;
            mistralrs_argv(row, config, model_path, port, home, &exe, MISTRALRS_BUILT)
        }
    }
}

/// The `local-worker` command of this binary for a `mistralrs` row. `built` says whether the
/// worker is compiled in. Flags are checked first, so a bad row reports its flag, then the
/// feature, then the gate.
fn mistralrs_argv(
    row: &RegistryRow,
    config: &LocalServingConfig,
    model_path: &Path,
    port: u16,
    home: Option<&str>,
    exe: &Path,
    built: bool,
) -> Result<Vec<String>, String> {
    let extra = extra_flags(row, home)?;
    let opts: MistralrsOptions = mistralrs_flags::parse(&row.id, row.ctx, &extra)?;
    if !built {
        return Err(MISTRALRS_NOT_BUILT.to_owned());
    }
    check_gate(config, &row.id, model_path)?;
    let mut argv = vec![
        utf8(exe, row, "llm-hub binary")?,
        WORKER_SUBCOMMAND.to_owned(),
        "--id".to_owned(),
        row.id.clone(),
        "--model".to_owned(),
        utf8(model_path, row, "model path")?,
        "--port".to_owned(),
        port.to_string(),
    ];
    argv.extend(opts.worker_args());
    Ok(argv)
}

/// The `cmd_sync_swap` command of `llama-models`, with the row's `extra_flags` last so they win.
fn llama_argv(
    row: &RegistryRow,
    llama_bin: &Path,
    model_path: &Path,
    port: u16,
    home: Option<&str>,
) -> Result<Vec<String>, String> {
    let extra = extra_flags(row, home)?;
    let mut argv = match &row.bin_override {
        Some(bin) => vec![ManagedCommand::expand_home(bin.clone(), home)],
        None => vec![
            utf8(llama_bin, row, "llama binary")?,
            LLAMA_SERVE.to_owned(),
        ],
    };
    argv.extend([
        "-m".to_owned(),
        utf8(model_path, row, "model path")?,
        "--port".to_owned(),
        port.to_string(),
        "--ctx-size".to_owned(),
        row.ctx.to_string(),
    ]);
    argv.extend(SHARED_BEFORE_GPU.map(str::to_owned));
    if !sets_gpu_layers(&extra) {
        argv.extend(SHARED_GPU_LAYERS.map(str::to_owned));
    }
    argv.extend(SHARED_AFTER_GPU.map(str::to_owned));
    argv.extend(extra);
    Ok(argv)
}

fn extra_flags(row: &RegistryRow, home: Option<&str>) -> Result<Vec<String>, String> {
    if row.extra_flags.trim().is_empty() {
        return Ok(Vec::new());
    }
    ManagedCommand::parse_argv(&row.extra_flags, home)
        .map_err(|reason| format!("model {}: extra_flags {reason}", row.id))
}

/// True when the row's flags hold `--gpu-layers`, `--n-gpu-layers` or `-ngl`, also as `flag=value`.
fn sets_gpu_layers(extra: &[String]) -> bool {
    extra.iter().any(|word| {
        let flag = word.split_once('=').map_or(word.as_str(), |(flag, _)| flag);
        GPU_LAYER_FLAGS.contains(&flag)
    })
}

fn utf8(path: &Path, row: &RegistryRow, what: &str) -> Result<String, String> {
    path.to_str().map(str::to_owned).ok_or_else(|| {
        format!(
            "model {}: the {what} {} is not valid UTF-8",
            row.id,
            path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::path::PathBuf;
    use std::time::Duration;

    use super::*;

    const HOME: &str = "/Users/me";

    fn config() -> LocalServingConfig {
        LocalServingConfig {
            registry: PathBuf::from("/r.txt"),
            memory_gb: None,
            idle_timeout: None,
            model_idle_timeouts: HashMap::new(),
            start_timeout: Duration::from_secs(1),
            llama_bin: PathBuf::from("/Users/me/.local/bin/llama"),
            models_dir: PathBuf::from("/Users/me/models"),
            hf_cache: PathBuf::from("/Users/me/hf"),
            gate_file: PathBuf::from("/Users/me/gates.jsonl"),
            ungated_models: HashSet::new(),
            allow_ungated: false,
        }
    }

    fn row(id: &str, extra_flags: &str, bin_override: Option<&str>) -> RegistryRow {
        RegistryRow {
            id: id.to_owned(),
            repo: "org/repo".to_owned(),
            file: "file.gguf".to_owned(),
            size_gb: 6.0,
            ctx: 131_072,
            quant: "Q4_K_M".to_owned(),
            extra_flags: extra_flags.to_owned(),
            bin_override: bin_override.map(str::to_owned),
            engine: Engine::Llama,
        }
    }

    fn argv(row: &RegistryRow) -> Result<Vec<String>, String> {
        let model = PathBuf::from(format!("/Users/me/models/{}.gguf", row.id));
        build_argv(row, &config(), &model, 5800, Some(HOME))
    }

    /// The shared tuning after `--port`, with the GPU flag spliced in when `gpu` is set.
    fn shared(gpu: bool) -> Vec<&'static str> {
        let mut words = vec!["--ctx-size", "131072"];
        words.extend(SHARED_BEFORE_GPU);
        if gpu {
            words.extend(SHARED_GPU_LAYERS);
        }
        words.extend(SHARED_AFTER_GPU);
        words
    }

    #[test]
    fn plain_row_matches_the_llama_swap_command() {
        // The "gemma-4-e4b-uncensored" entry of ~/.config/llama-swap/config.yaml, PORT=5800.
        let row = row(
            "gemma-4-e4b-uncensored",
            "--jinja --reasoning off --cache-type-k q8_0 --cache-type-v q8_0 --cache-reuse 0",
            None,
        );
        let expected = [
            "/Users/me/.local/bin/llama",
            "serve",
            "-m",
            "/Users/me/models/gemma-4-e4b-uncensored.gguf",
            "--port",
            "5800",
            "--ctx-size",
            "131072",
            "--parallel",
            "1",
            "--batch-size",
            "4096",
            "--ubatch-size",
            "2048",
            "--cache-reuse",
            "256",
            "--flash-attn",
            "on",
            "--gpu-layers",
            "99",
            "--metrics",
            "--temp",
            "1.0",
            "--top-p",
            "0.95",
            "--top-k",
            "40",
            "--min-p",
            "0.01",
            "--repeat-penalty",
            "1.0",
            "--jinja",
            "--reasoning",
            "off",
            "--cache-type-k",
            "q8_0",
            "--cache-type-v",
            "q8_0",
            "--cache-reuse",
            "0",
        ];
        assert_eq!(argv(&row).unwrap(), expected);
    }

    #[cfg(unix)]
    #[test]
    fn bin_override_runs_without_serve_and_keeps_quoted_flags_whole() {
        // The "muse-glimmer-30b" row: its own --n-gpu-layers replaces the shared one.
        let row = row(
            "muse-glimmer-30b",
            r#"--n-gpu-layers 99 --jinja --chat-template-kwargs '{"reasoning_strength":"low"}'"#,
            Some("~/opt/llama.cpp-src/build/bin/llama-server"),
        );
        let mut expected = vec![
            "/Users/me/opt/llama.cpp-src/build/bin/llama-server",
            "-m",
            "/Users/me/models/muse-glimmer-30b.gguf",
            "--port",
            "5800",
        ];
        expected.extend(shared(false));
        expected.extend([
            "--n-gpu-layers",
            "99",
            "--jinja",
            "--chat-template-kwargs",
            r#"{"reasoning_strength":"low"}"#,
        ]);
        assert_eq!(argv(&row).unwrap(), expected);
    }

    #[test]
    fn a_row_gpu_flag_in_any_spelling_skips_the_shared_one() {
        for flags in ["-ngl 40", "--gpu-layers 12", "--n-gpu-layers=7"] {
            let built = argv(&row("m", flags, None)).unwrap();
            let gpu_flags = built
                .iter()
                .filter(|word| sets_gpu_layers(std::slice::from_ref(word)))
                .count();
            assert_eq!(gpu_flags, 1, "{flags}: {built:?}");
            assert!(built.ends_with(&flags_words(flags)), "{flags}: {built:?}");
        }
    }

    fn flags_words(flags: &str) -> Vec<String> {
        flags.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn a_row_spec_type_is_the_only_one() {
        let built = argv(&row("m", "--spec-type ngram-cache", None)).unwrap();
        let spec = built.iter().filter(|word| *word == "--spec-type").count();
        assert_eq!(spec, 1);
        assert!(built.ends_with(&flags_words("--spec-type ngram-cache")));
    }

    #[test]
    fn empty_extra_flags_end_with_the_shared_tuning() {
        let mut expected = vec![
            "/Users/me/.local/bin/llama",
            "serve",
            "-m",
            "/Users/me/models/m.gguf",
            "--port",
            "5800",
        ];
        expected.extend(shared(true));
        assert_eq!(argv(&row("m", "  ", None)).unwrap(), expected);
    }

    #[test]
    fn unbalanced_quotes_name_the_model() {
        let err = argv(&row("m", r#"--alias "open"#, None)).unwrap_err();
        assert_eq!(err, "model m: extra_flags has unbalanced quotes");
    }

    /// A config whose gate file is `gate`, with `m` listed as ungated when `ungated`.
    fn mistral_config(gate: &Path, ungated: bool) -> LocalServingConfig {
        LocalServingConfig {
            gate_file: gate.to_path_buf(),
            ungated_models: if ungated {
                HashSet::from(["m".to_owned()])
            } else {
                HashSet::new()
            },
            ..config()
        }
    }

    fn mistral_row(flags: &str) -> RegistryRow {
        RegistryRow {
            engine: Engine::Mistralrs,
            ..row("m", flags, None)
        }
    }

    #[test]
    fn mistralrs_argv_runs_the_worker_of_this_binary() {
        let model = PathBuf::from("/models/m.gguf");
        let cfg = mistral_config(Path::new("/none.jsonl"), true);
        let built = mistralrs_argv(
            &mistral_row("--reasoning off --mmproj /m/proj.gguf --tok-model-id org/x"),
            &cfg,
            &model,
            5801,
            Some(HOME),
            Path::new("/bin/llm-hub"),
            true,
        )
        .unwrap();
        let mut expected =
            words("/bin/llm-hub local-worker --id m --model /models/m.gguf --port 5801");
        expected.extend(words(
            "--ctx-size 131072 --temp 1.0 --top-p 0.95 --top-k 40 --min-p 0.01 \
             --reasoning off --mmproj /m/proj.gguf --tok-model-id org/x",
        ));
        assert_eq!(built, expected);
    }

    fn words(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn mistralrs_row_without_the_feature_names_it_after_the_flags() {
        let model = PathBuf::from("/models/m.gguf");
        let cfg = mistral_config(Path::new("/none.jsonl"), true);
        let err = mistralrs_argv(
            &mistral_row(""),
            &cfg,
            &model,
            1,
            None,
            Path::new("/bin/llm-hub"),
            false,
        )
        .unwrap_err();
        assert_eq!(err, MISTRALRS_NOT_BUILT);
        let flag_err = mistralrs_argv(
            &mistral_row("--spec-type ngram-cache"),
            &cfg,
            &model,
            1,
            None,
            Path::new("/bin/llm-hub"),
            false,
        )
        .unwrap_err();
        assert!(
            flag_err.contains("--spec-type is not supported"),
            "{flag_err}"
        );
    }

    #[test]
    fn mistralrs_row_without_a_passed_gate_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("m.gguf");
        std::fs::write(&model, b"x").unwrap();
        let cfg = mistral_config(&dir.path().join("gates.jsonl"), false);
        let err = mistralrs_argv(
            &mistral_row(""),
            &cfg,
            &model,
            1,
            None,
            Path::new("/bin/llm-hub"),
            true,
        )
        .unwrap_err();
        assert!(
            err.starts_with("model m: the mistralrs engine refuses to start"),
            "{err}"
        );
    }

    #[test]
    fn mistralrs_row_with_a_passed_gate_starts() {
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("m.gguf");
        std::fs::write(&model, b"x").unwrap();
        let meta = std::fs::metadata(&model).unwrap();
        let mtime = meta
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let gate = dir.path().join("gates.jsonl");
        std::fs::write(
            &gate,
            serde_json::json!({
                "model": "m",
                "path": model.display().to_string(),
                "size": meta.len(),
                "mtime": mtime,
                "verdict": "pass",
            })
            .to_string(),
        )
        .unwrap();
        let cfg = mistral_config(&gate, false);
        let built = mistralrs_argv(
            &mistral_row(""),
            &cfg,
            &model,
            7,
            None,
            Path::new("/bin/llm-hub"),
            true,
        )
        .unwrap();
        assert_eq!(built[1], WORKER_SUBCOMMAND);
    }

    #[test]
    fn build_argv_dispatches_mistralrs_rows() {
        let model = PathBuf::from("/models/m.gguf");
        let cfg = mistral_config(Path::new("/none.jsonl"), true);
        let result = build_argv(&mistral_row(""), &cfg, &model, 1, None);
        // The test binary has no worker, so the feature check decides the outcome.
        match result {
            Ok(argv) => assert_eq!(argv[1], WORKER_SUBCOMMAND),
            Err(err) => assert_eq!(err, MISTRALRS_NOT_BUILT),
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_model_path_is_an_error() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let model = Path::new(OsStr::from_bytes(b"/models/\xff.gguf"));
        let err = build_argv(&row("m", "", None), &config(), model, 1, None).unwrap_err();
        assert!(
            err.starts_with("model m: the model path ") && err.ends_with(" is not valid UTF-8"),
            "{err}"
        );
    }
}
