//! `llm-hub smart-auto export`: writes one workflow's selection history into a generated block of a Markdown file.
//! Only the text between the start and end markers changes; a git-tracked target is refused in favour of a companion file.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Serialize;

use crate::consts::SELECTION_HISTORY_MAX_LIMIT;
use crate::schemas::selection_event::SelectionEvent;
use crate::services::launch::hub_client::HubClient;
use crate::services::launch::hub_endpoint::HubEndpoint;
use crate::utils::time::utc_iso8601;

const USAGE: &str = "usage: llm-hub smart-auto export --workflow <id> --out <path>";
const EXPORT_VERB: &str = "export";
const WORKFLOW_FLAG: &str = "--workflow";
const OUT_FLAG: &str = "--out";
const START_MARKER: &str = "<!-- llm-hub:smart-auto-export:start -->";
const END_MARKER: &str = "<!-- llm-hub:smart-auto-export:end -->";
const COMPANION_SUFFIX: &str = ".smart-auto.local.md";
const FALLBACK_STEM: &str = "smart-auto";
/// Symlinks followed before a path counts as a loop.
const MAX_SYMLINK_HOPS: usize = 40;
/// Events requested per history page; the hub clamps larger pages to the same limit.
const PAGE_LIMIT: usize = SELECTION_HISTORY_MAX_LIMIT;
/// Characters of a salted worker key shown in the table.
const WORKER_KEY_CHARS: usize = 8;
const EMPTY_CELL: &str = "-";
/// Length of `YYYY-MM-DDTHH:MM:SS`, the RFC 3339 prefix before the milliseconds.
const DATE_TIME_CHARS: usize = 19;

/// The parsed arguments of `llm-hub smart-auto export`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportArgs {
    pub workflow: String,
    pub out: PathBuf,
}

/// Parses the arguments after `llm-hub smart-auto`, such as `export --workflow <id> --out <path>`.
///
/// # Errors
/// Returns the usage line for a missing verb, a missing or empty flag, or an unknown argument.
pub fn parse_export_args(args: &[String]) -> Result<ExportArgs, String> {
    let [verb, rest @ ..] = args else {
        return Err(USAGE.to_string());
    };
    if verb != EXPORT_VERB {
        return Err(USAGE.to_string());
    }
    let mut workflow = None;
    let mut out = None;
    let mut iter = rest.iter();
    while let Some(arg) = iter.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) => (flag, Some(value.to_string())),
            None => (arg.as_str(), None),
        };
        let slot = match flag {
            WORKFLOW_FLAG => &mut workflow,
            OUT_FLAG => &mut out,
            _ => return Err(format!("unknown argument '{arg}'\n{USAGE}")),
        };
        let value = match inline {
            Some(value) => value,
            None => iter
                .next()
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value\n{USAGE}"))?,
        };
        *slot = Some(value);
    }
    let workflow = workflow
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{WORKFLOW_FLAG} is required\n{USAGE}"))?;
    let out = out
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| format!("{OUT_FLAG} is required\n{USAGE}"))?;
    Ok(ExportArgs { workflow, out })
}

/// Runs `llm-hub smart-auto <args>`: fetches the workflow's history from the hub and writes its block.
///
/// # Errors
/// Returns a message for bad arguments, an unreachable hub, a refused target or an unwritable file.
pub async fn run(args: &[String]) -> Result<String, String> {
    let export = parse_export_args(args)?;
    let client = HubClient::new(HubEndpoint::from_lookup(|name| std::env::var(name).ok()))?;
    let events = fetch_history(&client, &export.workflow).await?;
    let target = resolve_target(&export.out)?;
    let existing = match std::fs::read_to_string(&target) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("cannot read {}: {e}", target.display())),
    };
    let contents = splice_block(
        existing.as_deref(),
        &render_block(&export.workflow, &events),
    )?;
    std::fs::write(&target, contents)
        .map_err(|e| format!("cannot write {}: {e}", target.display()))?;
    Ok(format!(
        "wrote {} events to {}",
        events.len(),
        target.display()
    ))
}

/// Every event of `workflow`, oldest first, read page by page until a short page ends the history.
async fn fetch_history(client: &HubClient, workflow: &str) -> Result<Vec<SelectionEvent>, String> {
    let mut events = Vec::new();
    let mut since_seq = 0;
    loop {
        let page = client
            .smart_auto_history(workflow, since_seq, PAGE_LIMIT)
            .await?;
        let Some(last) = page.last() else {
            break;
        };
        since_seq = last.seq;
        let full_page = page.len() == PAGE_LIMIT;
        events.extend(page);
        if !full_page {
            break;
        }
    }
    Ok(events)
}

/// The file to write: symlinks followed to their target, with the directory canonicalized.
///
/// # Errors
/// Returns a message when the path cannot be resolved, names a directory, or is git-tracked.
/// The git-tracked message names the private companion file to use instead.
pub fn resolve_target(out: &Path) -> Result<PathBuf, String> {
    let target = resolve_symlinks(out)?;
    if is_git_tracked(&target) {
        return Err(format!(
            "refusing to write git-tracked file {}. Use the private companion file {} instead.",
            target.display(),
            companion_path(&target).display()
        ));
    }
    Ok(target)
}

/// Follows each symlink in `path` to its target, then canonicalizes the directory of the result.
fn resolve_symlinks(path: &Path) -> Result<PathBuf, String> {
    let mut current = path.to_path_buf();
    for _ in 0..=MAX_SYMLINK_HOPS {
        let is_link =
            std::fs::symlink_metadata(&current).is_ok_and(|meta| meta.file_type().is_symlink());
        if !is_link {
            return finish_resolution(path, &current);
        }
        let link = std::fs::read_link(&current)
            .map_err(|e| format!("cannot read symlink {}: {e}", current.display()))?;
        current = match current.parent() {
            Some(parent) if link.is_relative() => parent.join(link),
            _ => link,
        };
    }
    Err(format!(
        "too many symlinks while resolving {}",
        path.display()
    ))
}

/// Canonicalizes the directory of a non-symlink `current` and rejects a directory target.
fn finish_resolution(original: &Path, current: &Path) -> Result<PathBuf, String> {
    let parent = current
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = current
        .file_name()
        .ok_or_else(|| format!("{} has no file name", original.display()))?;
    let directory = std::fs::canonicalize(parent)
        .map_err(|e| format!("cannot resolve {}: {e}", parent.display()))?;
    let resolved = directory.join(name);
    if resolved.is_dir() {
        return Err(format!("{} is a directory", resolved.display()));
    }
    Ok(resolved)
}

/// True when git has `file` in its index. A file outside a repository is untracked.
fn is_git_tracked(file: &Path) -> bool {
    let (Some(directory), Some(name)) = (file.parent(), file.file_name()) else {
        return false;
    };
    Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(["ls-files", "--error-unmatch", "--"])
        .arg(name)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// The untracked sibling of a tracked file, named `<stem>.smart-auto.local.md`.
fn companion_path(target: &Path) -> PathBuf {
    let stem = target
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(FALLBACK_STEM);
    target.with_file_name(format!("{stem}{COMPANION_SUFFIX}"))
}

/// The generated block for `events`, from the start marker through the end marker, with a trailing newline.
pub fn render_block(workflow: &str, events: &[SelectionEvent]) -> String {
    let mut lines = vec![
        START_MARKER.to_string(),
        "<!-- Generated by `llm-hub smart-auto export`; edits inside this block are overwritten. -->".to_string(),
        format!("## Smart Auto history: {}", escape_cell(workflow)),
        String::new(),
    ];
    if events.is_empty() {
        lines.push("No selection events recorded for this workflow yet.".to_string());
    } else {
        lines.push(
            "| seq | worker | time (UTC) | stage | model | state | reason | role | effort | status | latency ms | tokens in / out |"
                .to_string(),
        );
        lines.push("|---|---|---|---|---|---|---|---|---|---|---|---|".to_string());
        lines.extend(events.iter().map(table_row));
    }
    lines.push(String::new());
    lines.push(END_MARKER.to_string());
    let mut block = lines.join("\n");
    block.push('\n');
    block
}

/// One Markdown table row for `event`. It holds only the scope keys and metrics the event carries.
fn table_row(event: &SelectionEvent) -> String {
    let cells = [
        event.seq.to_string(),
        event
            .worker
            .as_deref()
            .map_or_else(|| EMPTY_CELL.to_string(), short_key),
        format_utc(event.ts_ms),
        snake_name(&event.stage),
        event.model.clone(),
        snake_name(&event.state),
        event
            .reason
            .clone()
            .unwrap_or_else(|| EMPTY_CELL.to_string()),
        event
            .role
            .map_or_else(|| EMPTY_CELL.to_string(), |role| role.as_str().to_string()),
        event.sent_effort.or(event.selected_effort).map_or_else(
            || EMPTY_CELL.to_string(),
            |effort| effort.as_str().to_string(),
        ),
        event
            .status
            .map_or_else(|| EMPTY_CELL.to_string(), |status| status.to_string()),
        event
            .latency_ms
            .map_or_else(|| EMPTY_CELL.to_string(), |latency| latency.to_string()),
        format!(
            "{} / {}",
            optional_count(event.tokens_in),
            optional_count(event.tokens_out)
        ),
    ];
    let row = cells
        .iter()
        .map(|value| escape_cell(value))
        .collect::<Vec<_>>()
        .join(" | ");
    format!("| {row} |")
}

fn optional_count(value: Option<u64>) -> String {
    value.map_or_else(|| EMPTY_CELL.to_string(), |count| count.to_string())
}

fn short_key(key: &str) -> String {
    key.chars().take(WORKER_KEY_CHARS).collect()
}

/// The serde `snake_case` name of a unit enum value.
fn snake_name<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|json| json.as_str().map(str::to_owned))
        .unwrap_or_else(|| EMPTY_CELL.to_string())
}

/// Keeps a value on one table cell: pipes are escaped and line breaks become spaces.
fn escape_cell(value: &str) -> String {
    value.replace('|', "\\|").replace(['\n', '\r'], " ")
}

/// `text` with the generated block replaced by `block`, or with `block` appended when no block exists.
/// The block is written without its trailing newline and the text around it is kept byte for byte.
///
/// # Errors
/// Returns a message when the markers are not exactly one start and one end marker in that order.
pub fn splice_block(existing: Option<&str>, block: &str) -> Result<String, String> {
    let body = block.trim_end_matches('\n');
    let Some(text) = existing else {
        return Ok(format!("{body}\n"));
    };
    match (text.find(START_MARKER), text.find(END_MARKER)) {
        (None, None) => {
            let mut out = text.to_string();
            if !out.is_empty() {
                if !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push('\n');
            }
            out.push_str(body);
            out.push('\n');
            Ok(out)
        }
        (Some(start), Some(end))
            if start < end
                && text.matches(START_MARKER).count() == 1
                && text.matches(END_MARKER).count() == 1 =>
        {
            let mut out = String::with_capacity(text.len() + body.len());
            out.push_str(&text[..start]);
            out.push_str(body);
            out.push_str(&text[end + END_MARKER.len()..]);
            Ok(out)
        }
        _ => Err(format!(
            "generated block markers are malformed: expected one {START_MARKER} and one {END_MARKER}, in that order"
        )),
    }
}

/// `YYYY-MM-DD HH:MM:SS` in UTC for Unix milliseconds, from the shared RFC 3339 formatter.
fn format_utc(ts_ms: u64) -> String {
    utc_iso8601(ts_ms)[..DATE_TIME_CHARS].replace('T', " ")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;
    use wiremock::matchers::{path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::schemas::effort_level::EffortLevel;
    use crate::schemas::selection_stage::SelectionStage;
    use crate::schemas::selection_state::SelectionState;
    use crate::schemas::work_role::WorkRole;

    const EXISTING_PLAN: &str = "# Plan\n\n- [ ] S42 open task\n- [x] S43 done task\n";

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|value| (*value).to_string()).collect()
    }

    fn event(seq: u64) -> SelectionEvent {
        let mut event = SelectionEvent::new(
            SelectionStage::Solver,
            SelectionState::Completed,
            "copilot/model-a",
        );
        event.seq = seq;
        event.ts_ms = 1_700_000_000_000;
        event.worker = Some("abcdef0123456789".to_string());
        event.role = Some(WorkRole::Implementation);
        event.sent_effort = Some(EffortLevel::High);
        event.status = Some(200);
        event.latency_ms = Some(420);
        event.tokens_in = Some(1000);
        event.tokens_out = Some(50);
        event
    }

    fn client_for(url: &str) -> HubClient {
        let endpoint = HubEndpoint::from_lookup(|name| match name {
            "LLM_HUB_URL" => Some(url.to_string()),
            "LLM_HUB_API_KEY" => Some("key-1".to_string()),
            _ => None,
        });
        HubClient::new(endpoint).unwrap()
    }

    #[test]
    fn parse_accepts_both_flag_forms() {
        let parsed = parse_export_args(&args(&[
            "export",
            "--workflow",
            "wf-1",
            "--out=agent-spec/plan.md",
        ]))
        .unwrap();

        assert_eq!(
            parsed,
            ExportArgs {
                workflow: "wf-1".to_string(),
                out: PathBuf::from("agent-spec/plan.md"),
            }
        );
    }

    #[test]
    fn parse_rejects_missing_empty_and_unknown_arguments() {
        for list in [
            vec!["import", "--workflow", "w", "--out", "x.md"],
            vec!["export", "--out", "x.md"],
            vec!["export", "--workflow", "w"],
            vec!["export", "--workflow", "", "--out", "x.md"],
            vec!["export", "--workflow"],
            vec!["export", "--workflow", "w", "--out", "x.md", "--bogus"],
            vec![],
        ] {
            assert!(parse_export_args(&args(&list)).is_err(), "{list:?}");
        }
    }

    #[test]
    fn utc_formatting_matches_known_instants() {
        assert_eq!(format_utc(0), "1970-01-01 00:00:00");
        assert_eq!(format_utc(1_700_000_000_000), "2023-11-14 22:13:20");
        assert_eq!(format_utc(951_782_400_000), "2000-02-29 00:00:00");
    }

    #[test]
    fn missing_file_gets_only_the_block() {
        let block = render_block("wf-1", &[event(1)]);

        assert_eq!(splice_block(None, &block).unwrap(), block);
    }

    #[test]
    fn append_keeps_existing_text_and_checkboxes() {
        let block = render_block("wf-1", &[event(1)]);

        let out = splice_block(Some(EXISTING_PLAN), &block).unwrap();

        assert!(out.starts_with(EXISTING_PLAN));
        assert!(out.contains("- [ ] S42 open task"));
        assert!(out.contains("- [x] S43 done task"));
        assert!(out.ends_with(&format!("{END_MARKER}\n")));
    }

    #[test]
    fn rewrite_is_idempotent_and_replaces_only_the_block() {
        let first = splice_block(Some(EXISTING_PLAN), &render_block("wf-1", &[event(1)])).unwrap();
        let second = splice_block(Some(&first), &render_block("wf-1", &[event(1)])).unwrap();
        let wrapped = format!("intro\n\n{first}\n- [ ] trailing task\n");

        let rewritten = splice_block(Some(&wrapped), &render_block("wf-1", &[event(2)])).unwrap();

        assert_eq!(first, second);
        assert!(rewritten.starts_with("intro\n\n"));
        assert!(rewritten.contains("| 2 |"));
        assert!(!rewritten.contains("| 1 |"));
        assert!(rewritten.ends_with("- [ ] trailing task\n"));
    }

    #[test]
    fn malformed_markers_are_refused() {
        let block = render_block("wf-1", &[]);
        let only_start = format!("{START_MARKER}\nnever closed\n");
        let reversed = format!("{END_MARKER}\n{START_MARKER}\n");
        let doubled = format!("{block}{block}");

        for text in [only_start.as_str(), reversed.as_str(), doubled.as_str()] {
            assert!(splice_block(Some(text), &block).is_err(), "{text}");
        }
    }

    #[test]
    fn block_has_one_row_per_event_and_escapes_cells() {
        let mut escaped = event(2);
        escaped.reason = Some("a|b".to_string());

        let block = render_block("wf-1", &[event(1), escaped]);

        assert!(block.starts_with(START_MARKER));
        assert!(block.contains("| 1 | abcdef01 | 2023-11-14 22:13:20 | solver | copilot/model-a | completed | - | implementation | high | 200 | 420 | 1000 / 50 |"));
        assert!(block.contains("a\\|b"));
        assert!(block.trim_end().ends_with(END_MARKER));
    }

    #[test]
    fn empty_history_says_so() {
        assert!(render_block("wf-1", &[]).contains("No selection events recorded"));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_resolves_to_its_target() {
        let dir = tempdir().unwrap();
        let real = dir.path().join("real.md");
        fs::write(&real, EXISTING_PLAN).unwrap();
        let link = dir.path().join("link.md");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        assert_eq!(
            resolve_target(&link).unwrap(),
            fs::canonicalize(&real).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlink_resolves_to_a_new_file_at_its_target() {
        let dir = tempdir().unwrap();
        let link = dir.path().join("link.md");
        std::os::unix::fs::symlink(dir.path().join("new.md"), &link).unwrap();

        assert_eq!(
            resolve_target(&link).unwrap(),
            fs::canonicalize(dir.path()).unwrap().join("new.md")
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_loop_is_an_error() {
        let dir = tempdir().unwrap();
        let first = dir.path().join("a.md");
        let second = dir.path().join("b.md");
        std::os::unix::fs::symlink(&second, &first).unwrap();
        std::os::unix::fs::symlink(&first, &second).unwrap();

        assert!(resolve_target(&first).is_err());
    }

    #[test]
    fn untracked_file_outside_a_repository_is_allowed() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("untracked.md");
        fs::write(&file, EXISTING_PLAN).unwrap();

        assert!(resolve_target(&file).is_ok());
    }

    #[test]
    fn git_tracked_file_is_refused_with_the_companion_path() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        let init = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["init", "-q"])
            .status();
        assert!(init.is_ok_and(|status| status.success()));
        let file = repo.join("tracked.md");
        fs::write(&file, EXISTING_PLAN).unwrap();
        let add = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["add", "tracked.md"])
            .status();
        assert!(add.is_ok_and(|status| status.success()));

        let error = resolve_target(&file).unwrap_err();

        assert!(error.contains("refusing to write git-tracked file"));
        assert!(error.contains("tracked.smart-auto.local.md"));
    }

    #[tokio::test]
    async fn history_is_read_until_a_short_page() {
        let server = MockServer::start().await;
        let page = serde_json::json!({"events": [serde_json::to_value(event(3)).unwrap()]});
        Mock::given(path("/api/smart-auto/history"))
            .and(query_param("since_seq", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(page))
            .expect(1)
            .mount(&server)
            .await;

        let events = fetch_history(&client_for(&server.uri()), "wf-1")
            .await
            .unwrap();

        assert_eq!(events, [event(3)]);
    }
}
