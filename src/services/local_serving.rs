//! Native local serving: the hub starts one inference process per registry model on demand.

pub mod command;
pub mod memory;
pub mod mistralrs_flags;
pub mod pool;
pub mod registry_rows;
pub mod worker;

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::Deserialize;

use crate::configs::LocalServingConfig;

/// One line of the gate JSONL file. A record needs `model`, `verdict`, `size` and `mtime`. A
/// record with `path` also needs the file at that path.
#[derive(Debug, Deserialize)]
struct GateRecord {
    model: String,
    verdict: String,
    path: Option<PathBuf>,
    size: Option<u64>,
    mtime: Option<u64>,
}

/// True when `text` (gate JSONL) holds a `pass` record for model `id` whose `size` and `mtime`
/// match the model file at `path`, and whose `path` (when it has one) is `path`. A replaced
/// file fails even at the same path. Lines that do not parse are skipped.
pub fn gate_passed(text: &str, id: &str, path: &Path, size: u64, mtime: Option<u64>) -> bool {
    let Some(mtime) = mtime else {
        return false;
    };
    text.lines()
        .filter_map(|line| serde_json::from_str::<GateRecord>(line).ok())
        .any(|record| {
            record.model == id
                && record.verdict == "pass"
                && record.size == Some(size)
                && record.mtime == Some(mtime)
                && record
                    .path
                    .as_deref()
                    .is_none_or(|recorded| recorded == path)
        })
}

/// Refuses a `mistralrs` row that has no passed gate for its model file. The row passes when
/// its id is in `UNGATED_MODELS` or `ALLOW_UNGATED` is set.
///
/// # Errors
///
/// Returns a message naming the id, the file and the gate file when the gate is missing, or
/// when the model file or the gate file cannot be read.
pub fn check_gate(config: &LocalServingConfig, id: &str, model_path: &Path) -> Result<(), String> {
    if config.allow_ungated || config.ungated_models.contains(id) {
        return Ok(());
    }
    let meta = std::fs::metadata(model_path)
        .map_err(|err| format!("model {id}: cannot stat {}: {err}", model_path.display()))?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|age| age.as_secs());
    let gate_file = &config.gate_file;
    let text = match std::fs::read_to_string(gate_file) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => {
            return Err(format!(
                "model {id}: cannot read the gate file {}: {err}",
                gate_file.display()
            ));
        }
    };
    if gate_passed(&text, id, model_path, meta.len(), mtime) {
        return Ok(());
    }
    Err(format!(
        "model {id}: the mistralrs engine refuses to start: no passed gate for {} in {}; \
         run the gate, add {id} to UNGATED_MODELS or set ALLOW_UNGATED=true",
        model_path.display(),
        gate_file.display()
    ))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use super::*;

    const PATH: &str = "/models/m.gguf";

    fn config(dir: &Path, ungated: &[&str], allow: bool) -> LocalServingConfig {
        LocalServingConfig {
            registry: PathBuf::from("/r.txt"),
            memory_gb: None,
            idle_timeout: None,
            model_idle_timeouts: HashMap::new(),
            start_timeout: Duration::from_secs(1),
            llama_bin: PathBuf::from("llama"),
            models_dir: dir.to_path_buf(),
            hf_cache: dir.to_path_buf(),
            gate_file: dir.join("gates.jsonl"),
            ungated_models: ungated.iter().map(|id| (*id).to_owned()).collect(),
            allow_ungated: allow,
        }
    }

    /// A model file of `len` bytes in `dir`, returned with its path.
    fn model(dir: &Path, len: usize) -> PathBuf {
        let path = dir.join("m.gguf");
        std::fs::write(&path, vec![0_u8; len]).unwrap();
        path
    }

    #[test]
    fn a_pass_record_with_a_path_needs_the_size_and_mtime_of_the_file() {
        let text =
            format!(r#"{{"model":"m","path":"{PATH}","size":7,"mtime":9,"verdict":"pass"}}"#);
        assert!(gate_passed(&text, "m", Path::new(PATH), 7, Some(9)));
        assert!(
            !gate_passed(&text, "m", Path::new(PATH), 8, Some(9)),
            "size changed"
        );
        assert!(
            !gate_passed(&text, "m", Path::new(PATH), 7, Some(10)),
            "mtime changed"
        );
        assert!(!gate_passed(&text, "m", Path::new(PATH), 7, None));
        let path_only = format!(r#"{{"model":"m","path":"{PATH}","verdict":"pass"}}"#);
        assert!(!gate_passed(&path_only, "m", Path::new(PATH), 7, Some(9)));
    }

    #[test]
    fn a_pass_record_without_a_path_matches_by_size_and_mtime() {
        let text = r#"{"model":"m","size":7,"mtime":9,"verdict":"pass"}"#;
        assert!(gate_passed(text, "m", Path::new(PATH), 7, Some(9)));
        assert!(!gate_passed(text, "m", Path::new(PATH), 8, Some(9)));
        assert!(!gate_passed(text, "m", Path::new(PATH), 7, None));
    }

    #[test]
    fn a_pass_record_with_another_path_does_not_pass() {
        let text = r#"{"model":"m","path":"/old/m.gguf","size":7,"mtime":9,"verdict":"pass"}"#;
        assert!(!gate_passed(text, "m", Path::new(PATH), 7, Some(9)));
    }

    #[test]
    fn a_fail_record_or_another_model_does_not_pass() {
        let fail = format!(r#"{{"model":"m","path":"{PATH}","verdict":"fail"}}"#);
        let other = format!(r#"{{"model":"other","path":"{PATH}","verdict":"pass"}}"#);
        let text = format!("{fail}\n{other}\nnot json\n");
        assert!(!gate_passed(&text, "m", Path::new(PATH), 1, None));
        assert!(!gate_passed("", "m", Path::new(PATH), 1, None));
    }

    #[test]
    fn check_gate_passes_with_a_matching_gate_record_and_fails_a_replaced_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = model(dir.path(), 3);
        let cfg = config(dir.path(), &[], false);
        let meta = std::fs::metadata(&path).unwrap();
        let mtime = meta
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        std::fs::write(
            &cfg.gate_file,
            serde_json::json!({
                "model": "m",
                "path": path.display().to_string(),
                "size": meta.len(),
                "mtime": mtime,
                "verdict": "pass",
            })
            .to_string(),
        )
        .unwrap();
        assert_eq!(check_gate(&cfg, "m", &path), Ok(()));
        std::fs::write(&path, vec![0_u8; 5]).unwrap();
        assert!(
            check_gate(&cfg, "m", &path).is_err(),
            "a replaced file fails"
        );
    }

    #[test]
    fn check_gate_refuses_without_a_gate_file_and_names_the_way_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = model(dir.path(), 3);
        let cfg = config(dir.path(), &[], false);
        let err = check_gate(&cfg, "m", &path).unwrap_err();
        assert!(
            err.starts_with("model m: the mistralrs engine refuses to start"),
            "{err}"
        );
        assert!(
            err.contains("UNGATED_MODELS") && err.contains("ALLOW_UNGATED"),
            "{err}"
        );
    }

    #[test]
    fn check_gate_passes_for_an_ungated_id_or_allow_ungated() {
        let dir = tempfile::tempdir().unwrap();
        let path = model(dir.path(), 3);
        let listed = config(dir.path(), &["m"], false);
        assert_eq!(check_gate(&listed, "m", &path), Ok(()));
        let all = config(dir.path(), &[], true);
        assert_eq!(check_gate(&all, "m", &path), Ok(()));
    }

    #[test]
    fn check_gate_reports_a_missing_model_file() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path(), &[], false);
        let err = check_gate(&cfg, "m", &dir.path().join("absent.gguf")).unwrap_err();
        assert!(err.starts_with("model m: cannot stat "), "{err}");
    }

    #[test]
    fn check_gate_reports_an_unreadable_gate_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = model(dir.path(), 3);
        let cfg = LocalServingConfig {
            gate_file: dir.path().to_path_buf(),
            ..config(dir.path(), &[], false)
        };
        let err = check_gate(&cfg, "m", &path).unwrap_err();
        assert!(
            err.starts_with("model m: cannot read the gate file "),
            "{err}"
        );
    }
}
