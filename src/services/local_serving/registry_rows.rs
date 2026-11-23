//! Reads rows `id|repo|file|size_gb|ctx|quant|extra_flags|bin_override|engine` of a model
//! registry and finds each model's GGUF file on disk.
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::services::local_tuner::registry::row_id;

const MIN_COLS: usize = 5;
const GGUF_EXTENSION: &str = "gguf";

/// The inference engine that serves a registry row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Engine {
    /// llama.cpp's `llama-server`, the default when column 9 is empty or missing.
    #[default]
    Llama,
    /// The in-process mistral.rs worker.
    Mistralrs,
}

impl Engine {
    /// The registry and API name of this engine.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Llama => "llama",
            Self::Mistralrs => "mistralrs",
        }
    }

    /// Parses an engine name from an API request or registry value.
    pub fn from_name(value: &str) -> Result<Self, String> {
        match value.trim() {
            "llama" => Ok(Self::Llama),
            "mistralrs" => Ok(Self::Mistralrs),
            other => Err(format!(
                "unknown engine {other:?}; expected llama or mistralrs"
            )),
        }
    }
}

/// One model row of the registry, with its columns trimmed.
#[derive(Debug, Clone, PartialEq)]
pub struct RegistryRow {
    /// Column 1: the model id clients ask for.
    pub id: String,
    /// Column 2: the Hugging Face repo, `<org>/<repo>`.
    pub repo: String,
    /// Column 3: the GGUF file name inside the repo.
    pub file: String,
    /// Column 4: the approximate size of the weights in GB.
    pub size_gb: f64,
    /// Column 5: the context size in tokens.
    pub ctx: u64,
    /// Column 6: the quantization tag, empty when the row omits it.
    pub quant: String,
    /// Column 7: extra server flags, empty when the row omits them.
    pub extra_flags: String,
    /// Column 8: the server binary that replaces the default one.
    pub bin_override: Option<String>,
    /// Column 9: the engine that serves the row.
    pub engine: Engine,
}

/// The rows of a registry and a warning for each line the reader skipped.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedRegistry {
    /// The valid rows in file order, one per id.
    pub rows: Vec<RegistryRow>,
    /// One message per skipped line, naming the line, the id and the bad value.
    pub warnings: Vec<String>,
}

/// Parses registry text. The reader skips `#` comments and blank lines without a warning. It
/// skips a short row, a bad number, an unknown engine and a repeated id with a warning.
pub fn parse(text: &str) -> ParsedRegistry {
    let mut parsed = ParsedRegistry::default();
    let mut seen = HashSet::new();
    for (index, line) in text.lines().enumerate() {
        let Some(id) = row_id(line) else {
            continue;
        };
        let line_no = index + 1;
        let row = match parse_row(line) {
            Ok(row) => row,
            Err(reason) => {
                parsed.warnings.push(format!(
                    "registry line {line_no} (id {id:?}): {reason}; the row is skipped"
                ));
                continue;
            }
        };
        if seen.insert(id) {
            parsed.rows.push(row);
        } else {
            parsed.warnings.push(format!(
                "registry line {line_no}: duplicate id {id:?}; the first row wins"
            ));
        }
    }
    parsed
}

/// Reads and parses the registry at `path`, and logs each warning.
///
/// # Errors
///
/// Returns the I/O error when the file cannot be read as UTF-8 text.
pub fn read(path: &Path) -> std::io::Result<Vec<RegistryRow>> {
    let parsed = parse(&std::fs::read_to_string(path)?);
    for warning in &parsed.warnings {
        tracing::warn!(registry = %path.display(), "{warning}");
    }
    Ok(parsed.rows)
}

/// Sets column 9 of model `id` and writes the registry through an atomic rename.
///
/// # Errors
///
/// Returns an error when the registry cannot be read or written, or the model row is unknown.
pub fn set_engine(path: &Path, id: &str, engine: Engine) -> Result<(), String> {
    let target =
        std::fs::canonicalize(path).map_err(|err| format!("cannot read registry: {err}"))?;
    let original =
        std::fs::read_to_string(&target).map_err(|err| format!("cannot read registry: {err}"))?;
    let permissions = std::fs::metadata(&target)
        .map_err(|err| format!("cannot inspect registry: {err}"))?
        .permissions();
    let mut out = String::with_capacity(original.len() + 16);
    let mut found = false;
    for line in original.split_inclusive('\n') {
        let (body, ending) = split_ending(line);
        if !found && row_id(body) == Some(id) {
            let cols: Vec<&str> = body.split('|').collect();
            if cols.len() < MIN_COLS {
                return Err(format!(
                    "model {id}: registry row has {} columns, expected at least {MIN_COLS}",
                    cols.len()
                ));
            }
            let mut edited = cols;
            if edited.len() < 9 {
                edited.resize(9, "");
            }
            edited[8] = engine.as_str();
            out.push_str(&edited.join("|"));
            found = true;
        } else {
            out.push_str(body);
        }
        out.push_str(ending);
    }
    if !found {
        return Err(format!("unknown model {id:?}"));
    }
    if out != original {
        crate::utils::atomic_write::write_atomically(&target, out.as_bytes())
            .map_err(|err| format!("cannot write registry: {err}"))?;
        std::fs::set_permissions(&target, permissions)
            .map_err(|err| format!("cannot preserve registry permissions: {err}"))?;
    }
    Ok(())
}

fn split_ending(line: &str) -> (&str, &str) {
    let body = line.strip_suffix('\n').unwrap_or(line);
    let body = body.strip_suffix('\r').unwrap_or(body);
    (body, &line[body.len()..])
}

/// Finds the model file of `row`: `<models_dir>/<id>.gguf`, else the first snapshot in
/// `<hf_cache>/models--<org>--<repo>/snapshots/` that holds `<file>`.
///
/// # Errors
///
/// Returns a message that names both places when neither holds the file.
pub fn resolve_model_file(
    row: &RegistryRow,
    models_dir: &Path,
    hf_cache: &Path,
) -> Result<PathBuf, String> {
    let linked = models_dir.join(format!("{}.{GGUF_EXTENSION}", row.id));
    if linked.is_file() {
        return Ok(linked);
    }
    let snapshots = hf_cache
        .join(format!("models--{}", row.repo.replace('/', "--")))
        .join("snapshots");
    if let Some(found) = find_in_snapshots(&snapshots, &row.file) {
        return Ok(found);
    }
    Err(format!(
        "model {} is not on disk: neither {} nor {} exists",
        row.id,
        linked.display(),
        snapshots.join("*").join(&row.file).display()
    ))
}

/// Returns `<snapshot>/<file>` of the first snapshot by name that holds the file.
fn find_in_snapshots(snapshots: &Path, file: &str) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(snapshots)
        .ok()?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    dirs.sort();
    dirs.into_iter()
        .map(|dir| dir.join(file))
        .find(|candidate| candidate.is_file())
}

fn parse_row(line: &str) -> Result<RegistryRow, String> {
    let cols: Vec<&str> = line.split('|').map(str::trim).collect();
    if cols.len() < MIN_COLS {
        return Err(format!(
            "row has {} columns, expected at least {MIN_COLS}",
            cols.len()
        ));
    }
    let optional = |index: usize| cols.get(index).copied().unwrap_or_default();
    if cols[0].is_empty() {
        return Err("empty id".to_string());
    }
    let size_gb = cols[3]
        .parse::<f64>()
        .ok()
        .filter(|size| size.is_finite() && *size >= 0.0)
        .ok_or_else(|| format!("invalid size_gb {:?}", cols[3]))?;
    let ctx = cols[4]
        .parse::<u64>()
        .map_err(|_| format!("invalid ctx {:?}", cols[4]))?;
    let engine = parse_engine(optional(8))?;
    let bin_override = Some(optional(7))
        .filter(|bin| !bin.is_empty())
        .map(str::to_string);
    Ok(RegistryRow {
        id: cols[0].to_string(),
        repo: cols[1].to_string(),
        file: cols[2].to_string(),
        size_gb,
        ctx,
        quant: optional(5).to_string(),
        extra_flags: optional(6).to_string(),
        bin_override,
        engine,
    })
}

fn parse_engine(value: &str) -> Result<Engine, String> {
    match value {
        "" | "llama" => Ok(Engine::Llama),
        "mistralrs" => Ok(Engine::Mistralrs),
        other => Err(format!("unknown engine {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, repo: &str, file: &str) -> RegistryRow {
        RegistryRow {
            id: id.to_string(),
            repo: repo.to_string(),
            file: file.to_string(),
            size_gb: 1.0,
            ctx: 4096,
            quant: String::new(),
            extra_flags: String::new(),
            bin_override: None,
            engine: Engine::Llama,
        }
    }

    #[test]
    fn parses_rows_with_five_to_nine_columns() {
        let text = "# id|repo|file|size_gb|ctx\n\
            \n\
            a|org/a|a.gguf|4.3|8192\n\
            b | org/b | b.gguf | 17 | 65536 | Q4_K_M | --jinja --temp 0.6 \n\
            c|org/c|c.gguf|6|262144|Q8_0||~/bin/llama-server\n\
            d|org/d|d.gguf|6|1024|Q8_0|--jinja||mistralrs\n\
            e|org/e|e.gguf|6|1024|Q8_0|||llama\n";
        let parsed = parse(text);
        assert_eq!(parsed.warnings, Vec::<String>::new());
        let ids: Vec<&str> = parsed.rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c", "d", "e"]);
        let a = &parsed.rows[0];
        assert!((a.size_gb - 4.3).abs() < f64::EPSILON);
        assert_eq!(
            (a.ctx, a.quant.as_str(), a.extra_flags.as_str()),
            (8192, "", "")
        );
        assert_eq!((a.bin_override.as_deref(), a.engine), (None, Engine::Llama));
        let b = &parsed.rows[1];
        assert_eq!(
            (b.repo.as_str(), b.file.as_str(), b.quant.as_str()),
            ("org/b", "b.gguf", "Q4_K_M")
        );
        assert_eq!(b.extra_flags, "--jinja --temp 0.6");
        assert_eq!(
            parsed.rows[2].bin_override.as_deref(),
            Some("~/bin/llama-server")
        );
        assert_eq!(parsed.rows[2].extra_flags, "");
        assert_eq!(parsed.rows[3].engine, Engine::Mistralrs);
        assert_eq!(parsed.rows[3].bin_override, None);
        assert_eq!(parsed.rows[4].engine, Engine::Llama);
    }

    #[test]
    fn set_engine_updates_only_column_nine_and_keeps_other_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.registry");
        std::fs::write(
            &path,
            "# keep this\na|org/a|a.gguf|4.3|8192|Q4_K_M|--jinja|~/llama|llama\r\nb|org/b|b.gguf|1|4096|||llama-server|llama\n",
        )
        .unwrap();

        set_engine(&path, "a", Engine::Mistralrs).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            "# keep this\na|org/a|a.gguf|4.3|8192|Q4_K_M|--jinja|~/llama|mistralrs\r\nb|org/b|b.gguf|1|4096|||llama-server|llama\n"
        );
    }

    #[test]
    fn set_engine_rejects_an_unknown_model() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.registry");
        std::fs::write(&path, "a|org/a|a.gguf|1|4096\n").unwrap();

        let err = set_engine(&path, "missing", Engine::Llama).unwrap_err();

        assert_eq!(err, "unknown model \"missing\"");
    }

    #[test]
    fn unknown_engine_skips_the_row_with_a_warning() {
        let parsed = parse("ok|o/r|f.gguf|1|2\nbad|o/r|f.gguf|1|2|q|||vllm\n");
        assert_eq!(parsed.rows.len(), 1);
        assert_eq!(
            parsed.warnings,
            ["registry line 2 (id \"bad\"): unknown engine \"vllm\"; the row is skipped"]
        );
    }

    #[test]
    fn duplicate_id_keeps_the_first_row() {
        let parsed = parse("m|o/first|f.gguf|1|2\n# m|x|y|1|2\nm|o/second|f.gguf|1|2\n");
        assert_eq!(parsed.rows.len(), 1);
        assert_eq!(parsed.rows[0].repo, "o/first");
        assert_eq!(
            parsed.warnings,
            ["registry line 3: duplicate id \"m\"; the first row wins"]
        );
    }

    #[test]
    fn invalid_rows_are_skipped_with_a_reason() {
        let text = "short|o/r|f.gguf|1\n\
            |o/r|f.gguf|1|2\n\
            size|o/r|f.gguf|big|2\n\
            neg|o/r|f.gguf|-1|2\n\
            ctx|o/r|f.gguf|1|-5\n\
            good|o/r|f.gguf|1|2\n";
        let parsed = parse(text);
        assert_eq!(parsed.rows.len(), 1);
        assert_eq!(parsed.rows[0].id, "good");
        assert_eq!(
            parsed.warnings,
            [
                "registry line 1 (id \"short\"): row has 4 columns, expected at least 5; the row is skipped",
                "registry line 2 (id \"\"): empty id; the row is skipped",
                "registry line 3 (id \"size\"): invalid size_gb \"big\"; the row is skipped",
                "registry line 4 (id \"neg\"): invalid size_gb \"-1\"; the row is skipped",
                "registry line 5 (id \"ctx\"): invalid ctx \"-5\"; the row is skipped",
            ]
        );
    }

    #[test]
    fn invalid_row_does_not_claim_its_id() {
        let parsed = parse("m|o/r|f.gguf|1|2|q|||vllm\nm|o/r|f.gguf|1|2\n");
        assert_eq!(parsed.rows.len(), 1);
        assert_eq!(parsed.warnings.len(), 1);
    }

    #[test]
    fn read_returns_rows_or_the_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.registry");
        std::fs::write(&path, "m|o/r|f.gguf|1|2\nx|o/r|f.gguf|1|2|q|||bad\n").unwrap();
        let rows = read(&path).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "m");
        assert!(read(&dir.path().join("missing")).is_err());
    }

    #[test]
    fn models_dir_link_wins_over_the_hf_cache() {
        let dir = tempfile::tempdir().unwrap();
        let models = dir.path().join("models");
        let snapshot = dir.path().join("hub/models--org--repo/snapshots/abc");
        std::fs::create_dir_all(&models).unwrap();
        std::fs::create_dir_all(&snapshot).unwrap();
        std::fs::write(models.join("m.gguf"), b"x").unwrap();
        std::fs::write(snapshot.join("file.gguf"), b"x").unwrap();
        let found = resolve_model_file(
            &row("m", "org/repo", "file.gguf"),
            &models,
            &dir.path().join("hub"),
        );
        assert_eq!(found, Ok(models.join("m.gguf")));
    }

    #[test]
    fn falls_back_to_the_first_snapshot_that_holds_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let hub = dir.path().join("hub");
        let snapshots = hub.join("models--org--repo/snapshots");
        for name in ["a-empty", "b-has", "c-has"] {
            std::fs::create_dir_all(snapshots.join(name)).unwrap();
        }
        std::fs::write(snapshots.join("b-has/file.gguf"), b"x").unwrap();
        std::fs::write(snapshots.join("c-has/file.gguf"), b"x").unwrap();
        std::fs::write(snapshots.join("stray.txt"), b"x").unwrap();
        let found = resolve_model_file(
            &row("m", "org/repo", "file.gguf"),
            &dir.path().join("no-models"),
            &hub,
        );
        assert_eq!(found, Ok(snapshots.join("b-has/file.gguf")));
    }

    #[test]
    fn a_directory_named_like_the_model_is_not_a_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("m.gguf")).unwrap();
        let err = resolve_model_file(&row("m", "org/repo", "f.gguf"), dir.path(), dir.path());
        assert!(err.is_err());
    }

    #[test]
    fn missing_file_is_not_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let hub = dir.path().join("hub");
        std::fs::create_dir_all(hub.join("models--org--repo/snapshots/abc")).unwrap();
        let err =
            resolve_model_file(&row("m", "org/repo", "f.gguf"), dir.path(), &hub).unwrap_err();
        assert!(err.starts_with("model m is not on disk: neither "), "{err}");
        assert!(err.contains(&dir.path().join("m.gguf").display().to_string()));
        assert!(err.contains("models--org--repo"));
        assert!(err.ends_with("f.gguf exists"), "{err}");
    }
}
