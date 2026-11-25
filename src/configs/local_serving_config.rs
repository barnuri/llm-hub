use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::hub_config::{is_falsy, is_truthy};
use super::managed_command::ManagedCommand;
use crate::consts::{
    DEFAULT_LOCAL_GATE_FILE, DEFAULT_LOCAL_HF_CACHE, DEFAULT_LOCAL_IDLE_SECS,
    DEFAULT_LOCAL_LLAMA_BIN, DEFAULT_LOCAL_MODELS_DIR, DEFAULT_LOCAL_START_TIMEOUT_SECS,
    LOCAL_LLAMA_BIN_ON_PATH, LOCAL_PROFILE_KIND,
};

/// Settings of a profile with `LLM_HUB_<P>_KIND=local`, where the hub serves registry models
/// itself. They come only from the operator's `.env`; the admin API never writes them.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalServingConfig {
    /// `LLM_HUB_<P>_REGISTRY`, else the profile's `TUNER_REGISTRY`, with `~/` expanded.
    pub registry: PathBuf,
    /// `LLM_HUB_<P>_MEMORY_GB`. `None` leaves the budget to the runtime default.
    pub memory_gb: Option<f64>,
    /// `LLM_HUB_<P>_IDLE_MS`, else `LLM_HUB_<P>_IDLE_SECS`. `None` when it is `0`: a model is never
    /// stopped for being idle.
    pub idle_timeout: Option<Duration>,
    /// `LLM_HUB_<P>_MODEL_TTL_SECS` as `id=secs,...`: per-model idle timeouts, `0` means never.
    pub model_idle_timeouts: HashMap<String, Option<Duration>>,
    /// `LLM_HUB_<P>_START_TIMEOUT_MS`, else `LLM_HUB_<P>_START_TIMEOUT_SECS`: how long a model gets
    /// to become healthy.
    pub start_timeout: Duration,
    /// `LLM_HUB_<P>_LLAMA_BIN`, else `~/.local/bin/llama` when executable, else `llama` on PATH.
    pub llama_bin: PathBuf,
    /// `LLM_HUB_<P>_MODELS_DIR`: the folder of pinned `<id>.gguf` files.
    pub models_dir: PathBuf,
    /// `LLM_HUB_<P>_HF_CACHE`: the Hugging Face hub cache searched for a row's file.
    pub hf_cache: PathBuf,
    /// `LLM_HUB_<P>_GATE_FILE`: JSONL file of gate records that `mistralrs` rows must pass.
    pub gate_file: PathBuf,
    /// `LLM_HUB_<P>_UNGATED_MODELS`: ids of `mistralrs` rows that may start without a passed gate.
    pub ungated_models: HashSet<String>,
    /// `LLM_HUB_<P>_ALLOW_UNGATED`: serve every `mistralrs` row without a passed gate.
    pub allow_ungated: bool,
}

impl LocalServingConfig {
    /// Idle timeout of `model_id`: its `MODEL_TTL_SECS` entry, else the profile's `IDLE_SECS`.
    pub fn idle_timeout_for(&self, model_id: &str) -> Option<Duration> {
        self.model_idle_timeouts
            .get(model_id)
            .copied()
            .unwrap_or(self.idle_timeout)
    }
}

impl LocalServingConfig {
    /// Reads the local serving keys of profile `upper` through `get`, which takes a key without
    /// the `LLM_HUB_<P>_` prefix. Returns `None` when `KIND` is unset or empty.
    /// `tuner_registry` is the profile's parsed `TUNER_REGISTRY`.
    ///
    /// # Errors
    /// A message naming the key when `KIND` is not `local`, when a value does not parse, or
    /// when neither `REGISTRY` nor `TUNER_REGISTRY` is set.
    pub fn from_env(
        upper: &str,
        home: Option<&str>,
        get: &dyn Fn(&str) -> Option<String>,
        tuner_registry: Option<&Path>,
    ) -> Result<Option<Self>, String> {
        let present = |key: &str| get(key).filter(|v| !v.is_empty());
        let Some(kind) = present("KIND") else {
            return Ok(None);
        };
        if !kind.eq_ignore_ascii_case(LOCAL_PROFILE_KIND) {
            return Err(format!(
                "LLM_HUB_{upper}_KIND must be {LOCAL_PROFILE_KIND} or unset: {kind}"
            ));
        }
        let expand = |raw: String| PathBuf::from(ManagedCommand::expand_home(raw, home));
        let registry = present("REGISTRY")
            .map(expand)
            .or_else(|| tuner_registry.map(Path::to_path_buf))
            .ok_or_else(|| {
                format!(
                    "LLM_HUB_{upper}_KIND=local needs LLM_HUB_{upper}_REGISTRY or LLM_HUB_{upper}_TUNER_REGISTRY"
                )
            })?;
        let idle = ManagedCommand::parse_duration(upper, "IDLE", get, DEFAULT_LOCAL_IDLE_SECS)?;
        let start_timeout = ManagedCommand::parse_duration(
            upper,
            "START_TIMEOUT",
            get,
            DEFAULT_LOCAL_START_TIMEOUT_SECS,
        )?;
        Ok(Some(Self {
            registry,
            memory_gb: Self::memory_gb(upper, present("MEMORY_GB"))?,
            idle_timeout: (!idle.is_zero()).then_some(idle),
            model_idle_timeouts: Self::model_idle_timeouts(upper, present("MODEL_TTL_SECS"))?,
            start_timeout,
            llama_bin: present("LLAMA_BIN").map_or_else(|| Self::default_llama_bin(home), expand),
            models_dir: expand(
                present("MODELS_DIR").unwrap_or_else(|| DEFAULT_LOCAL_MODELS_DIR.into()),
            ),
            hf_cache: expand(present("HF_CACHE").unwrap_or_else(|| DEFAULT_LOCAL_HF_CACHE.into())),
            gate_file: expand(
                present("GATE_FILE").unwrap_or_else(|| DEFAULT_LOCAL_GATE_FILE.into()),
            ),
            ungated_models: present("UNGATED_MODELS").map_or_else(HashSet::new, |raw| {
                raw.split(',')
                    .map(str::trim)
                    .filter(|id| !id.is_empty())
                    .map(str::to_owned)
                    .collect()
            }),
            allow_ungated: Self::allow_ungated(upper, present("ALLOW_UNGATED"))?,
        }))
    }

    fn memory_gb(upper: &str, raw: Option<String>) -> Result<Option<f64>, String> {
        let Some(raw) = raw else {
            return Ok(None);
        };
        match raw.parse::<f64>() {
            Ok(gb) if gb.is_finite() && gb > 0.0 => Ok(Some(gb)),
            _ => Err(format!(
                "LLM_HUB_{upper}_MEMORY_GB must be a number of GB above 0: {raw}"
            )),
        }
    }

    fn model_idle_timeouts(
        upper: &str,
        raw: Option<String>,
    ) -> Result<HashMap<String, Option<Duration>>, String> {
        let Some(raw) = raw else {
            return Ok(HashMap::new());
        };
        raw.split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(|entry| {
                let parsed = entry.split_once('=').and_then(|(id, secs)| {
                    let id = id.trim();
                    let secs = secs.trim().parse::<u64>().ok()?;
                    (!id.is_empty()).then(|| (id.to_owned(), idle_timeout(secs)))
                });
                parsed.ok_or_else(|| {
                    format!(
                        "LLM_HUB_{upper}_MODEL_TTL_SECS entries must look like id=seconds: {entry}"
                    )
                })
            })
            .collect()
    }

    fn allow_ungated(upper: &str, raw: Option<String>) -> Result<bool, String> {
        match raw {
            None => Ok(false),
            Some(v) if is_truthy(&v) => Ok(true),
            Some(v) if is_falsy(&v) => Ok(false),
            Some(v) => Err(format!(
                "LLM_HUB_{upper}_ALLOW_UNGATED must be true or false: {v}"
            )),
        }
    }

    fn default_llama_bin(home: Option<&str>) -> PathBuf {
        let installed = PathBuf::from(ManagedCommand::expand_home(
            DEFAULT_LOCAL_LLAMA_BIN.to_owned(),
            home,
        ));
        if home.is_some() && is_executable(&installed) {
            installed
        } else {
            PathBuf::from(LOCAL_LLAMA_BIN_ON_PATH)
        }
    }
}

fn idle_timeout(secs: u64) -> Option<Duration> {
    (secs > 0).then(|| Duration::from_secs(secs))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "/home/op";

    fn parse_with(
        pairs: &[(&str, &str)],
        home: Option<&str>,
        tuner_registry: Option<&Path>,
    ) -> Result<Option<LocalServingConfig>, String> {
        let vars: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        LocalServingConfig::from_env(
            "LOCAL",
            home,
            &|key: &str| vars.get(key).cloned(),
            tuner_registry,
        )
    }

    fn parse(pairs: &[(&str, &str)]) -> Result<Option<LocalServingConfig>, String> {
        let mut with_kind = vec![("KIND", "local"), ("REGISTRY", "~/models/registry.txt")];
        with_kind.extend_from_slice(pairs);
        parse_with(&with_kind, Some(HOME), None)
    }

    #[test]
    fn unset_or_empty_kind_yields_no_config() {
        assert_eq!(parse_with(&[], Some(HOME), None).unwrap(), None);
        assert_eq!(parse_with(&[("KIND", "")], Some(HOME), None).unwrap(), None);
    }

    #[test]
    fn kind_local_fills_every_default() {
        let config = parse(&[]).unwrap().unwrap();
        assert_eq!(
            config.registry,
            PathBuf::from("/home/op/models/registry.txt")
        );
        assert_eq!(config.memory_gb, None);
        assert_eq!(config.idle_timeout, Some(Duration::from_mins(30)));
        assert!(config.model_idle_timeouts.is_empty());
        assert_eq!(config.start_timeout, Duration::from_mins(10));
        assert_eq!(config.llama_bin, PathBuf::from("llama"));
        assert_eq!(config.models_dir, PathBuf::from("/home/op/models"));
        assert_eq!(
            config.hf_cache,
            PathBuf::from("/home/op/.cache/huggingface/hub")
        );
        assert_eq!(
            config.gate_file,
            PathBuf::from("/home/op/.config/llama/mistralrs-gates.jsonl")
        );
        assert!(config.ungated_models.is_empty());
        assert!(!config.allow_ungated);
    }

    #[test]
    fn ungated_models_is_a_trimmed_comma_list() {
        let config = parse(&[("UNGATED_MODELS", " gemma-4-e4b, ,qwen-x ")])
            .unwrap()
            .unwrap();
        assert_eq!(
            config.ungated_models,
            HashSet::from(["gemma-4-e4b".to_owned(), "qwen-x".to_owned()])
        );
    }

    #[test]
    fn gate_file_expands_home_and_takes_an_override() {
        let config = parse(&[("GATE_FILE", "/data/gates.jsonl")])
            .unwrap()
            .unwrap();
        assert_eq!(config.gate_file, PathBuf::from("/data/gates.jsonl"));
        let config = parse(&[("GATE_FILE", "~/g.jsonl")]).unwrap().unwrap();
        assert_eq!(config.gate_file, PathBuf::from("/home/op/g.jsonl"));
    }

    #[test]
    fn kind_is_case_insensitive() {
        let config = parse_with(&[("KIND", "LOCAL"), ("REGISTRY", "/r.txt")], None, None).unwrap();
        assert!(config.is_some());
    }

    #[test]
    fn every_key_overrides_its_default() {
        let config = parse(&[
            ("REGISTRY", "/etc/registry.txt"),
            ("MEMORY_GB", "96.5"),
            ("IDLE_SECS", "60"),
            ("MODEL_TTL_SECS", " small=0 , big=120 ,"),
            ("START_TIMEOUT_SECS", "30"),
            ("LLAMA_BIN", "~/bin/llama-dev"),
            ("MODELS_DIR", "~/gguf"),
            ("HF_CACHE", "/data/hf"),
            ("GATE_FILE", "/etc/gates.jsonl"),
            ("UNGATED_MODELS", "a,b"),
            ("ALLOW_UNGATED", "true"),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(config.registry, PathBuf::from("/etc/registry.txt"));
        assert_eq!(config.memory_gb, Some(96.5));
        assert_eq!(config.idle_timeout, Some(Duration::from_mins(1)));
        assert_eq!(
            config.model_idle_timeouts,
            HashMap::from([
                ("small".to_owned(), None),
                ("big".to_owned(), Some(Duration::from_mins(2))),
            ])
        );
        assert_eq!(config.start_timeout, Duration::from_secs(30));
        assert_eq!(config.llama_bin, PathBuf::from("/home/op/bin/llama-dev"));
        assert_eq!(config.models_dir, PathBuf::from("/home/op/gguf"));
        assert_eq!(config.hf_cache, PathBuf::from("/data/hf"));
        assert_eq!(config.gate_file, PathBuf::from("/etc/gates.jsonl"));
        assert_eq!(config.ungated_models.len(), 2);
        assert!(config.allow_ungated);
    }

    #[test]
    fn idle_secs_zero_never_stops_and_model_ttl_wins_per_model() {
        let config = parse(&[("IDLE_SECS", "0"), ("MODEL_TTL_SECS", "big=90")])
            .unwrap()
            .unwrap();
        assert_eq!(config.idle_timeout, None);
        assert_eq!(config.idle_timeout_for("other"), None);
        assert_eq!(
            config.idle_timeout_for("big"),
            Some(Duration::from_secs(90))
        );
    }

    #[test]
    fn ms_keys_win_over_secs_keys() {
        let config = parse(&[
            ("IDLE_SECS", "60"),
            ("IDLE_MS", "150"),
            ("START_TIMEOUT_SECS", "30"),
            ("START_TIMEOUT_MS", "250"),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(config.idle_timeout, Some(Duration::from_millis(150)));
        assert_eq!(config.start_timeout, Duration::from_millis(250));
    }

    #[test]
    fn idle_ms_zero_never_stops() {
        let config = parse(&[("IDLE_MS", "0")]).unwrap().unwrap();
        assert_eq!(config.idle_timeout, None);
    }

    #[test]
    fn registry_falls_back_to_the_tuner_registry() {
        let tuner = PathBuf::from("/tuner/registry.txt");
        let config = parse_with(&[("KIND", "local")], Some(HOME), Some(&tuner))
            .unwrap()
            .unwrap();
        assert_eq!(config.registry, tuner);
    }

    #[test]
    fn missing_registry_names_both_keys() {
        let err = parse_with(&[("KIND", "local")], Some(HOME), None).unwrap_err();
        assert!(err.contains("LLM_HUB_LOCAL_REGISTRY"), "{err}");
        assert!(err.contains("LLM_HUB_LOCAL_TUNER_REGISTRY"), "{err}");
    }

    #[test]
    fn unknown_kind_is_rejected() {
        let err = parse_with(&[("KIND", "remote")], Some(HOME), None).unwrap_err();
        assert!(err.contains("LLM_HUB_LOCAL_KIND"), "{err}");
        assert!(err.contains("remote"), "{err}");
    }

    #[test]
    fn invalid_values_name_their_key() {
        for (key, value) in [
            ("MEMORY_GB", "lots"),
            ("MEMORY_GB", "0"),
            ("MEMORY_GB", "-4"),
            ("MEMORY_GB", "inf"),
            ("IDLE_SECS", "soon"),
            ("START_TIMEOUT_SECS", "-1"),
            ("IDLE_MS", "soon"),
            ("START_TIMEOUT_MS", "1.5"),
            ("MODEL_TTL_SECS", "big"),
            ("MODEL_TTL_SECS", "=60"),
            ("MODEL_TTL_SECS", "big=soon"),
            ("ALLOW_UNGATED", "maybe"),
        ] {
            let err = parse(&[(key, value)]).unwrap_err();
            assert!(
                err.contains(&format!("LLM_HUB_LOCAL_{key}")),
                "{key}={value}: {err}"
            );
        }
    }

    #[test]
    fn allow_ungated_reads_false_values() {
        for value in ["false", "0", "no", "OFF"] {
            let config = parse(&[("ALLOW_UNGATED", value)]).unwrap().unwrap();
            assert!(!config.allow_ungated, "{value}");
        }
    }

    #[test]
    fn without_home_tilde_paths_stay_unexpanded_and_llama_comes_from_path() {
        let config = parse_with(&[("KIND", "local"), ("REGISTRY", "/r.txt")], None, None)
            .unwrap()
            .unwrap();
        assert_eq!(config.models_dir, PathBuf::from("~/models"));
        assert_eq!(config.llama_bin, PathBuf::from("llama"));
    }

    #[cfg(unix)]
    #[test]
    fn an_executable_local_bin_llama_is_the_default_binary() {
        use std::os::unix::fs::PermissionsExt;

        let home = tempfile::tempdir().unwrap();
        let bin_dir = home.path().join(".local/bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let llama = bin_dir.join("llama");
        std::fs::write(&llama, "#!/bin/sh\n").unwrap();
        let home_str = home.path().to_str().unwrap();
        let pairs = [("KIND", "local"), ("REGISTRY", "/r.txt")];

        std::fs::set_permissions(&llama, std::fs::Permissions::from_mode(0o644)).unwrap();
        let not_executable = parse_with(&pairs, Some(home_str), None).unwrap().unwrap();
        assert_eq!(not_executable.llama_bin, PathBuf::from("llama"));

        std::fs::set_permissions(&llama, std::fs::Permissions::from_mode(0o755)).unwrap();
        let executable = parse_with(&pairs, Some(home_str), None).unwrap().unwrap();
        assert_eq!(executable.llama_bin, llama);
    }
}
