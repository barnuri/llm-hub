//! Edits rows `id|repo|file|size_gb|ctx|quant|extra_flags[|bin_override]` of a model registry.
//! The editor keeps `#` comments and blank lines byte for byte.
use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::schemas::notification::FlagDiff;
use crate::utils::atomic_write::write_atomically;

const ID_COL: usize = 0;
const CTX_COL: usize = 4;
const FLAGS_COL: usize = 6;
const MIN_COLS: usize = 7;

/// Why a registry edit failed.
#[derive(Debug)]
pub enum RegistryError {
    /// No row has the model id in column 1.
    NotFound(String),
    /// Reading the registry or writing it or its backup failed.
    Io(std::io::Error),
    /// The diff or the matched row cannot be written safely.
    Invalid(String),
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(model) => write!(f, "model {model} not found in the registry"),
            Self::Io(err) => write!(f, "registry io error: {err}"),
            Self::Invalid(reason) => write!(f, "invalid registry edit: {reason}"),
        }
    }
}

impl std::error::Error for RegistryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::NotFound(_) | Self::Invalid(_) => None,
        }
    }
}

impl From<std::io::Error> for RegistryError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

/// What `apply` did to the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyOutcome {
    /// True when the row changed and the file was rewritten.
    pub changed: bool,
    /// The backup of the original file, written only when the row changed.
    pub backup: Option<PathBuf>,
    /// Column 7 of the row after the edit.
    pub extra_flags: String,
    /// Column 5 of the row after the edit.
    pub ctx: String,
}

/// Applies `diff` to the row of `model`. A change writes a `<file>.bak-<now_s>[-<n>]` backup, then
/// the link target atomically with its permissions kept.
pub fn apply(
    path: &Path,
    model: &str,
    diff: &FlagDiff,
    now_s: u64,
) -> Result<ApplyOutcome, RegistryError> {
    validate(diff)?;
    let path = &std::fs::canonicalize(path)?;
    let original = std::fs::read_to_string(path)?;
    let mut out = String::with_capacity(original.len() + 64);
    let mut found: Option<(String, String)> = None;
    for line in original.split_inclusive('\n') {
        let (body, ending) = split_ending(line);
        if found.is_none() && row_id(body) == Some(model) {
            let edited = edit_row(body, diff)?;
            out.push_str(&edited.0);
            found = Some((edited.1, edited.2));
        } else {
            out.push_str(body);
        }
        out.push_str(ending);
    }
    let Some((extra_flags, ctx)) = found else {
        return Err(RegistryError::NotFound(model.to_string()));
    };
    let changed = out != original;
    let backup = if changed {
        let permissions = std::fs::metadata(path)?.permissions();
        let backup = write_backup(path, now_s, original.as_bytes())?;
        write_atomically(path, out.as_bytes())?;
        std::fs::set_permissions(path, permissions)?;
        Some(backup)
    } else {
        None
    };
    Ok(ApplyOutcome {
        changed,
        backup,
        extra_flags,
        ctx,
    })
}

/// Reads column 5 of the row of `model` as a context size. Returns `None` when the file,
/// the row or a numeric value is missing.
pub fn ctx_of(path: &Path, model: &str) -> Option<u64> {
    column_of(path, model, CTX_COL)?.trim().parse().ok()
}

fn column_of(path: &Path, model: &str, col: usize) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    text.lines()
        .find(|line| row_id(line) == Some(model))
        .and_then(|line| line.split('|').nth(col).map(str::to_string))
}

fn validate(diff: &FlagDiff) -> Result<(), RegistryError> {
    let bad = diff
        .add
        .iter()
        .chain(&diff.remove)
        .find(|value| value.contains(['|', '\n', '\r']));
    match bad {
        Some(value) => Err(RegistryError::Invalid(format!(
            "value {value:?} contains '|' or a newline"
        ))),
        None => Ok(()),
    }
}

/// Splits a line into its body and its `\n` or `\r\n` ending.
fn split_ending(line: &str) -> (&str, &str) {
    let body = line.strip_suffix('\n').unwrap_or(line);
    let body = body.strip_suffix('\r').unwrap_or(body);
    (body, &line[body.len()..])
}

/// Returns column 1 of a data row, or `None` for a comment or blank line.
fn row_id(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    line.split('|').nth(ID_COL).map(str::trim)
}

/// Returns the edited row with its new column 7 and column 5.
fn edit_row(body: &str, diff: &FlagDiff) -> Result<(String, String, String), RegistryError> {
    let mut cols: Vec<&str> = body.split('|').collect();
    if cols.len() < MIN_COLS {
        return Err(RegistryError::Invalid(format!(
            "row has {} columns, expected at least {MIN_COLS}",
            cols.len()
        )));
    }
    let flags = edit_flags(cols[FLAGS_COL], diff);
    let ctx = diff
        .ctx
        .map_or_else(|| cols[CTX_COL].to_string(), |ctx| ctx.to_string());
    cols[FLAGS_COL] = &flags;
    cols[CTX_COL] = &ctx;
    let row = cols.join("|");
    Ok((row, flags, ctx))
}

/// Strips each `remove` flag with its values, then appends the `add` tokens unless the
/// flags already end with them. It keeps the column untouched when nothing changes.
fn edit_flags(current: &str, diff: &FlagDiff) -> String {
    let mut tokens: Vec<&str> = current.split_whitespace().collect();
    let before = tokens.len();
    for flag in diff.remove.iter().map(|f| f.trim()).filter(|f| is_flag(f)) {
        tokens = strip_flag(&tokens, flag);
    }
    let add: Vec<&str> = diff.add.iter().flat_map(|a| a.split_whitespace()).collect();
    let append = !add.is_empty() && !tokens.ends_with(&add);
    if tokens.len() == before && !append {
        return current.to_string();
    }
    if append {
        tokens.extend(add);
    }
    tokens.join(" ")
}

/// Removes every occurrence of `flag` together with the value tokens that follow it.
fn strip_flag<'a>(tokens: &[&'a str], flag: &str) -> Vec<&'a str> {
    let mut kept = Vec::with_capacity(tokens.len());
    let mut skipping = false;
    for &token in tokens {
        if is_flag(token) {
            skipping = token == flag;
        }
        if !skipping {
            kept.push(token);
        }
    }
    kept
}

/// A flag starts with `-` and a non-digit, so a negative number like `-1` stays a value.
fn is_flag(token: &str) -> bool {
    let mut chars = token.chars();
    chars.next() == Some('-') && chars.next().is_some_and(|c| !c.is_ascii_digit())
}

/// Writes `bytes` to the first free name of `<file>.bak-<now_s>`, `<file>.bak-<now_s>-1`, …,
/// so two edits in one second keep both backups.
fn write_backup(path: &Path, now_s: u64, bytes: &[u8]) -> Result<PathBuf, RegistryError> {
    let mut attempt = 0_u32;
    loop {
        let backup = backup_path(path, now_s, attempt);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&backup)
        {
            Ok(mut file) => {
                file.write_all(bytes)?;
                return Ok(backup);
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                attempt = attempt.checked_add(1).ok_or(RegistryError::Io(err))?;
            }
            Err(err) => return Err(err.into()),
        }
    }
}

fn backup_path(path: &Path, now_s: u64, attempt: u32) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".bak-{now_s}"));
    if attempt > 0 {
        name.push(format!("-{attempt}"));
    }
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    const REGISTRY: &str = "# id|repo|file|size_gb|ctx|quant|extra_flags|bin\n\
        \n\
        gemma|org/gemma|gemma.gguf|17|65536|Q4_K_M|--flash-attn on --jinja\n\
        qwen|org/qwen|qwen.gguf|20|32768|Q4_K_M|--cache-reuse 256 -ngl -1|/opt/llama-server\n\
        # trailing comment\n";

    fn registry(content: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.txt");
        std::fs::write(&path, content).unwrap();
        (dir, path)
    }

    fn diff(add: &[&str], remove: &[&str], ctx: Option<u64>) -> FlagDiff {
        FlagDiff {
            add: add.iter().map(ToString::to_string).collect(),
            remove: remove.iter().map(ToString::to_string).collect(),
            ctx,
            old_ctx: None,
        }
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn missing_row_is_not_found_and_leaves_the_file() {
        let (_dir, path) = registry(REGISTRY);
        let err = apply(&path, "llama", &diff(&["--x"], &[], None), 1).unwrap_err();
        assert!(matches!(err, RegistryError::NotFound(ref m) if m == "llama"));
        assert_eq!(err.to_string(), "model llama not found in the registry");
        assert_eq!(read(&path), REGISTRY);
        assert!(!backup_path(&path, 1, 0).exists());
    }

    #[test]
    fn comment_matching_the_model_is_not_a_row() {
        let (_dir, path) = registry("# gemma|a|b|c|1|q|--x\n");
        let err = apply(&path, "gemma", &diff(&["--y"], &[], None), 1).unwrap_err();
        assert!(matches!(err, RegistryError::NotFound(_)));
    }

    #[test]
    fn appends_flags_and_keeps_comments_and_other_rows() {
        let (_dir, path) = registry(REGISTRY);
        let out = apply(
            &path,
            "gemma",
            &diff(&["--cache-type-k", "q8_0"], &[], None),
            7,
        )
        .unwrap();
        assert!(out.changed);
        assert_eq!(
            out.extra_flags,
            "--flash-attn on --jinja --cache-type-k q8_0"
        );
        assert_eq!(out.ctx, "65536");
        let expected = REGISTRY.replace(
            "--flash-attn on --jinja\n",
            "--flash-attn on --jinja --cache-type-k q8_0\n",
        );
        assert_eq!(read(&path), expected);
    }

    #[test]
    fn keeps_the_eighth_column() {
        let (_dir, path) = registry(REGISTRY);
        apply(&path, "qwen", &diff(&["--jinja"], &[], None), 1).unwrap();
        assert!(read(&path).contains(
            "qwen|org/qwen|qwen.gguf|20|32768|Q4_K_M|--cache-reuse 256 -ngl -1 --jinja|/opt/llama-server\n"
        ));
    }

    #[test]
    fn second_apply_is_a_no_op() {
        let (_dir, path) = registry(REGISTRY);
        let change = diff(&["--cache-type-k q8_0"], &["--cache-type-k"], Some(32_768));
        assert!(apply(&path, "gemma", &change, 1).unwrap().changed);
        let after_first = read(&path);
        let second = apply(&path, "gemma", &change, 2).unwrap();
        assert!(!second.changed);
        assert_eq!(second.backup, None);
        assert_eq!(read(&path), after_first);
        assert!(!backup_path(&path, 2, 0).exists());
    }

    #[test]
    fn removes_a_flag_with_its_value() {
        let (_dir, path) = registry(REGISTRY);
        let out = apply(
            &path,
            "qwen",
            &diff(&[], &["--cache-reuse", "256"], None),
            1,
        )
        .unwrap();
        assert_eq!(out.extra_flags, "-ngl -1");
        assert!(read(&path).contains("|Q4_K_M|-ngl -1|/opt/llama-server\n"));
    }

    #[test]
    fn removing_an_absent_flag_keeps_spacing() {
        let (_dir, path) = registry("m|r|f|1|2|q|--a  x\n");
        let out = apply(&path, "m", &diff(&[], &["--b"], None), 1).unwrap();
        assert!(!out.changed);
        assert_eq!(out.extra_flags, "--a  x");
    }

    #[test]
    fn sets_the_context_column() {
        let (_dir, path) = registry(REGISTRY);
        let out = apply(&path, "gemma", &diff(&[], &[], Some(32_768)), 1).unwrap();
        assert!(out.changed);
        assert_eq!(out.ctx, "32768");
        assert!(read(&path).contains("gemma|org/gemma|gemma.gguf|17|32768|Q4_K_M|"));
        assert_eq!(ctx_of(&path, "gemma"), Some(32_768));
    }

    #[test]
    fn writes_the_original_to_the_backup() {
        let (_dir, path) = registry(REGISTRY);
        let out = apply(
            &path,
            "gemma",
            &diff(&["--jinja2"], &[], None),
            1_700_000_000,
        )
        .unwrap();
        let backup = out.backup.unwrap();
        assert!(
            backup
                .to_string_lossy()
                .ends_with("models.txt.bak-1700000000")
        );
        assert_eq!(read(&backup), REGISTRY);
    }

    #[test]
    fn two_applies_in_one_second_keep_both_backups() {
        let (_dir, path) = registry(REGISTRY);
        let first = apply(&path, "gemma", &diff(&["--a"], &[], None), 5).unwrap();
        let after_first = read(&path);
        let second = apply(&path, "gemma", &diff(&["--b"], &[], None), 5).unwrap();
        let third = apply(&path, "gemma", &diff(&["--c"], &[], None), 5).unwrap();
        let names: Vec<String> = [first, second, third]
            .iter()
            .map(|o| o.backup.as_ref().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names[0].ends_with("models.txt.bak-5"));
        assert!(names[1].ends_with("models.txt.bak-5-1"));
        assert!(names[2].ends_with("models.txt.bak-5-2"));
        assert_eq!(read(Path::new(&names[0])), REGISTRY);
        assert_eq!(read(Path::new(&names[1])), after_first);
    }

    #[test]
    fn backup_dir_error_is_an_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.txt");
        std::fs::write(backup_path(&path, 1, 0), b"old").unwrap();
        std::fs::write(dir.path().join("models.txt.bak-1-1"), b"old").unwrap();
        let err = write_backup(&dir.path().join("missing").join("m"), 1, b"x").unwrap_err();
        assert!(matches!(err, RegistryError::Io(_)));
        let written = write_backup(&path, 1, b"x").unwrap();
        assert!(written.to_string_lossy().ends_with("models.txt.bak-1-2"));
    }

    #[cfg(unix)]
    #[test]
    fn follows_a_symlinked_registry_and_keeps_the_link_and_mode() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, target) = registry(REGISTRY);
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let out = apply(&link, "gemma", &diff(&["--x"], &[], None), 1).unwrap();
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(read(&target).contains("--flash-attn on --jinja --x\n"));
        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let backup = out.backup.unwrap();
        assert!(backup.to_string_lossy().ends_with("models.txt.bak-1"));
        assert_eq!(read(&backup), REGISTRY);
    }

    #[test]
    fn rejects_pipes_and_newlines() {
        let (_dir, path) = registry(REGISTRY);
        for bad in ["--a|b", "--a\nb", "--a\r"] {
            let err = apply(&path, "gemma", &diff(&[bad], &[], None), 1).unwrap_err();
            assert!(matches!(err, RegistryError::Invalid(_)), "{bad:?}");
            assert!(err.to_string().starts_with("invalid registry edit"));
        }
        let err = apply(&path, "gemma", &diff(&[], &["--a|"], None), 1).unwrap_err();
        assert!(matches!(err, RegistryError::Invalid(_)));
        assert_eq!(read(&path), REGISTRY);
    }

    #[test]
    fn rejects_a_short_row() {
        let (_dir, path) = registry("m|r|f|1|2|q\n");
        let err = apply(&path, "m", &diff(&["--x"], &[], None), 1).unwrap_err();
        assert!(matches!(err, RegistryError::Invalid(ref r) if r.contains("6 columns")));
    }

    #[test]
    fn missing_file_is_an_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = apply(&dir.path().join("none"), "m", &FlagDiff::default(), 1).unwrap_err();
        assert!(matches!(err, RegistryError::Io(_)));
        assert!(err.to_string().starts_with("registry io error"));
        assert!(std::error::Error::source(&err).is_some());
        assert!(std::error::Error::source(&RegistryError::NotFound("m".into())).is_none());
    }

    #[test]
    fn keeps_crlf_endings_and_a_missing_final_newline() {
        let (_dir, path) = registry("# c\r\nm|r|f|1|2|q|--a\r\nn|r|f|1|2|q|--b");
        apply(&path, "m", &diff(&["--x"], &[], None), 1).unwrap();
        apply(&path, "n", &diff(&["--y"], &[], None), 2).unwrap();
        assert_eq!(
            read(&path),
            "# c\r\nm|r|f|1|2|q|--a --x\r\nn|r|f|1|2|q|--b --y"
        );
    }

    #[test]
    fn appends_to_an_empty_flags_column() {
        let (_dir, path) = registry("m|r|f|1|2|q|\n");
        let out = apply(&path, "m", &diff(&["--x", "1"], &[], None), 1).unwrap();
        assert_eq!(out.extra_flags, "--x 1");
        assert_eq!(read(&path), "m|r|f|1|2|q|--x 1\n");
    }

    #[test]
    fn readers_return_columns_or_none() {
        let (_dir, path) = registry(REGISTRY);
        assert_eq!(ctx_of(&path, "qwen"), Some(32_768));
        assert_eq!(ctx_of(&path, "missing"), None);
        assert_eq!(ctx_of(&path.with_extension("none"), "gemma"), None);
        let (_dir2, bad) = registry("m|r|f|1|big|q|--a\n");
        assert_eq!(ctx_of(&bad, "m"), None);
    }

    #[test]
    fn negative_numbers_are_values_not_flags() {
        assert!(is_flag("--jinja"));
        assert!(is_flag("-ngl"));
        assert!(!is_flag("-1"));
        assert!(!is_flag("-"));
        assert!(!is_flag("q8_0"));
    }
}
