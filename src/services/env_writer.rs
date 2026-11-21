use std::path::Path;

use crate::configs::{ProfileConfig, env_name};
use crate::utils::atomic_write::write_atomically;

const PREFIX: &str = "LLM_HUB_";
/// Profile keys only an operator may set (they run a program or pick the
/// upstream credential); upsert keeps them as written and never writes them itself.
const OPERATOR_ONLY_KEYS: [&str; 15] = [
    "COMMAND",
    "FIRST_BYTE_TIMEOUT_MS",
    "IDLE_SECS",
    "START_TIMEOUT_SECS",
    "AUTH",
    "GITHUB_TOKEN_SOURCE",
    "GH_PATH",
    "GH_USER",
    "GITHUB_DOMAIN",
    "GITHUB_API_URL",
    "PREFILL_GUARD",
    "TUNER_REGISTRY",
    "TUNER_APPLY_COMMAND",
    "TUNER_APPLY_TIMEOUT_MS",
    "BUDGET_MONTHLY_USD",
];
/// Global keys a profile named `extensions`, `local`, `notify` or `budget` would otherwise claim as its own lines.
const GLOBAL_KEYS: [&str; 11] = [
    "EXTENSIONS_DISABLED",
    "LOCAL_TUNER",
    "NOTIFICATIONS",
    "NOTIFY_DESKTOP",
    "NOTIFY_WEBHOOK_URL",
    "NOTIFY_COMMAND",
    "NOTIFY_MIN_SEVERITY",
    "NOTIFY_TIMEOUT_MS",
    "NOTIFY_MUTE",
    "NOTIFY_BUDGET_STEPS",
    "BUDGET_MONTHLY_USD",
];

/// Rewrites the env file with a profile added/updated. Unrelated lines,
/// comments, and ordering are preserved; the write is temp-file + atomic
/// rename in the same directory (rename across filesystems is not atomic).
/// Operator-only lines (`OPERATOR_ONLY_KEYS`) survive.
pub fn upsert_profile(
    path: &Path,
    profile: &ProfileConfig,
    all_profile_names: &[String],
) -> Result<(), String> {
    let mut lines = read_lines(path)?;
    let upper = env_name(&profile.name);

    remove_profile_lines(&mut lines, &upper, &OPERATOR_ONLY_KEYS);
    set_var(&mut lines, "PROFILES", &all_profile_names.join(","));

    let mut new_vars: Vec<(String, String)> =
        vec![(format!("{upper}_BASE_URL"), profile.base_url.clone())];
    if let Some(label) = profile
        .display_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        new_vars.push((format!("{upper}_DISPLAY_NAME"), label.to_string()));
    }
    if !profile.api_key.is_empty() {
        new_vars.push((format!("{upper}_API_KEY"), profile.api_key.clone()));
    }
    if !profile.extra_headers.is_empty() {
        let obj: serde_json::Map<String, serde_json::Value> = profile
            .extra_headers
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect();
        new_vars.push((
            format!("{upper}_HEADERS"),
            serde_json::Value::Object(obj).to_string(),
        ));
    }
    if let Some(timeout) = profile.timeout_ms {
        new_vars.push((format!("{upper}_TIMEOUT_MS"), timeout.to_string()));
    }
    if !profile.enabled {
        new_vars.push((format!("{upper}_ENABLED"), "false".to_string()));
    }
    if !profile.keep_warm {
        new_vars.push((format!("{upper}_KEEP_WARM"), "false".to_string()));
    }
    if !profile.static_models.is_empty() {
        new_vars.push((format!("{upper}_MODELS"), profile.static_models.join(",")));
    }
    for (key, value) in new_vars {
        lines.push(format!("{PREFIX}{key}={value}"));
    }

    write_atomic(path, &lines)
}

/// Persists the default fallback chain (`LLM_HUB_DEFAULT_FALLBACKS`),
/// comma-joined. An empty chain clears the value.
pub fn set_default_fallbacks(path: &Path, chain: &[String]) -> Result<(), String> {
    let mut lines = read_lines(path)?;
    set_var(&mut lines, "DEFAULT_FALLBACKS", &chain.join(","));
    write_atomic(path, &lines)
}

/// Persists `LLM_HUB_KEEP_WARM` and `LLM_HUB_KEEP_WARM_IDLE_CAP_MINUTES`.
pub fn set_keep_warm(path: &Path, enabled: bool, idle_cap_minutes: u64) -> Result<(), String> {
    let mut lines = read_lines(path)?;
    set_var(
        &mut lines,
        "KEEP_WARM",
        if enabled { "true" } else { "false" },
    );
    set_var(
        &mut lines,
        "KEEP_WARM_IDLE_CAP_MINUTES",
        &idle_cap_minutes.to_string(),
    );
    write_atomic(path, &lines)
}

/// Persists `LLM_HUB_MEDIA_FALLBACK` and its vision and speech models. An empty model is saved
/// empty, which turns that conversion off.
pub fn set_media_fallback(
    path: &Path,
    enabled: bool,
    skip: bool,
    vision_model: &str,
    stt_model: &str,
) -> Result<(), String> {
    let flag = |on: bool| if on { "true" } else { "false" };
    let mut lines = read_lines(path)?;
    set_var(&mut lines, "MEDIA_FALLBACK", flag(enabled));
    set_var(&mut lines, "MEDIA_SKIP", flag(skip));
    set_var(&mut lines, "MEDIA_FALLBACK_VISION_MODEL", vision_model);
    set_var(&mut lines, "MEDIA_FALLBACK_STT_MODEL", stt_model);
    write_atomic(path, &lines)
}

/// Persists `LLM_HUB_COPILOT_PRICES_REFRESH_SECS`. `0` turns the price fetch off.
pub fn set_copilot_prices_refresh(path: &Path, secs: u64) -> Result<(), String> {
    let mut lines = read_lines(path)?;
    set_var(&mut lines, "COPILOT_PRICES_REFRESH_SECS", &secs.to_string());
    write_atomic(path, &lines)
}

pub fn remove_profile(
    path: &Path,
    name: &str,
    remaining_profile_names: &[String],
) -> Result<(), String> {
    let mut lines = read_lines(path)?;
    remove_profile_lines(&mut lines, &env_name(name), &[]);
    set_var(&mut lines, "PROFILES", &remaining_profile_names.join(","));
    write_atomic(path, &lines)
}

fn read_lines(path: &Path) -> Result<Vec<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(content.lines().map(str::to_string).collect()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

fn remove_profile_lines(lines: &mut Vec<String>, upper: &str, keep_keys: &[&str]) {
    let var_prefix = format!("{PREFIX}{upper}_");
    lines.retain(|line| {
        let trimmed = line.trim_start();
        if is_global_key(trimmed) {
            return true;
        }
        let Some(rest) = trimmed.strip_prefix(&var_prefix) else {
            return true;
        };
        keep_keys.iter().any(|key| {
            rest.strip_prefix(key)
                .is_some_and(|tail| tail.starts_with('='))
        })
    });
}

fn is_global_key(line: &str) -> bool {
    line.strip_prefix(PREFIX).is_some_and(|rest| {
        GLOBAL_KEYS.iter().any(|key| {
            rest.strip_prefix(key)
                .is_some_and(|tail| tail.starts_with('='))
        })
    })
}

/// Replaces `LLM_HUB_<key>=` in place if present, appends otherwise.
fn set_var(lines: &mut Vec<String>, key: &str, value: &str) {
    let full_key = format!("{PREFIX}{key}=");
    let new_line = format!("{PREFIX}{key}={value}");
    for line in lines.iter_mut() {
        if line.trim_start().starts_with(&full_key) {
            *line = new_line;
            return;
        }
    }
    lines.push(new_line);
}

fn write_atomic(path: &Path, lines: &[String]) -> Result<(), String> {
    let content = lines.join("\n") + "\n";
    write_atomically(path, content.as_bytes()).map_err(|e| format!("atomic env write failed: {e}"))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::configs::TokenRates;

    fn sample_profile() -> ProfileConfig {
        ProfileConfig {
            name: "groq".into(),
            display_name: Some("Groq Cloud".into()),
            base_url: "https://api.groq.com/openai/v1".into(),
            api_key: "gsk-12345".into(),
            extra_headers: vec![],
            timeout_ms: None,
            enabled: true,
            static_models: vec![],
            pricing: TokenRates::default(),
            model_prices: HashMap::default(),
            managed: None,
            copilot: None,
            keep_warm: true,
            first_byte_timeout_ms: None,
            tuner: crate::configs::LocalTunerConfig::default(),
            budget_monthly_usd: None,
        }
    }

    #[test]
    fn set_media_fallback_writes_the_switch_and_both_models() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");

        set_media_fallback(&path, true, false, "local/eye", "").unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            content,
            "LLM_HUB_MEDIA_FALLBACK=true\nLLM_HUB_MEDIA_SKIP=false\nLLM_HUB_MEDIA_FALLBACK_VISION_MODEL=local/eye\nLLM_HUB_MEDIA_FALLBACK_STT_MODEL=\n"
        );
    }

    #[test]
    fn set_keep_warm_writes_both_keys_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(&path, "# keep\nLLM_HUB_PROFILES=copilot\n").unwrap();

        set_keep_warm(&path, true, 30).unwrap();
        set_keep_warm(&path, false, 45).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            content,
            "# keep\nLLM_HUB_PROFILES=copilot\nLLM_HUB_KEEP_WARM=false\nLLM_HUB_KEEP_WARM_IDLE_CAP_MINUTES=45\n"
        );
    }

    #[test]
    fn set_copilot_prices_refresh_replaces_the_line_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(&path, "# keep\nLLM_HUB_PROFILES=copilot\n").unwrap();

        set_copilot_prices_refresh(&path, 3600).unwrap();
        set_copilot_prices_refresh(&path, 0).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            content,
            "# keep\nLLM_HUB_PROFILES=copilot\nLLM_HUB_COPILOT_PRICES_REFRESH_SECS=0\n"
        );
    }

    #[test]
    fn upsert_creates_and_updates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(
            &path,
            "# my comment\nLLM_HUB_PROFILES=openai\nLLM_HUB_OPENAI_BASE_URL=https://x\n",
        )
        .unwrap();

        let names = vec!["openai".to_string(), "groq".to_string()];
        upsert_profile(&path, &sample_profile(), &names).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.starts_with("# my comment\n"), "comment preserved");
        assert!(content.contains("LLM_HUB_PROFILES=openai,groq"));
        assert!(content.contains("LLM_HUB_GROQ_BASE_URL=https://api.groq.com/openai/v1"));
        assert!(content.contains("LLM_HUB_GROQ_DISPLAY_NAME=Groq Cloud"));
        assert!(content.contains("LLM_HUB_GROQ_API_KEY=gsk-12345"));

        // update: change key, ensure no duplicate lines
        let mut updated = sample_profile();
        updated.api_key = "gsk-99999".into();
        upsert_profile(&path, &updated, &names).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content.matches("LLM_HUB_GROQ_API_KEY=").count(), 1);
        assert!(content.contains("gsk-99999"));
    }

    #[test]
    fn upsert_existing_operator_command_kept_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(
            &path,
            "LLM_HUB_GROQ_BASE_URL=https://old\nLLM_HUB_GROQ_COMMAND=srv --port 1\nLLM_HUB_GROQ_IDLE_SECS=5\nLLM_HUB_GROQ_START_TIMEOUT_SECS=9\nLLM_HUB_GROQ_COMMANDX=dropped\n",
        )
        .unwrap();

        upsert_profile(&path, &sample_profile(), &["groq".to_string()]).unwrap();
        upsert_profile(&path, &sample_profile(), &["groq".to_string()]).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            content
                .matches("LLM_HUB_GROQ_COMMAND=srv --port 1\n")
                .count(),
            1
        );
        assert_eq!(content.matches("LLM_HUB_GROQ_IDLE_SECS=5\n").count(), 1);
        assert_eq!(
            content
                .matches("LLM_HUB_GROQ_START_TIMEOUT_SECS=9\n")
                .count(),
            1
        );
        assert!(!content.contains("COMMANDX"));
        assert!(!content.contains("https://old"));
    }

    #[test]
    fn upsert_keeps_copilot_auth_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(
            &path,
            "LLM_HUB_GROQ_BASE_URL=https://old\nLLM_HUB_GROQ_AUTH=github-copilot\nLLM_HUB_GROQ_GITHUB_TOKEN_SOURCE=gh\nLLM_HUB_GROQ_GH_PATH=/opt/bin/ghx\n",
        )
        .unwrap();

        upsert_profile(&path, &sample_profile(), &["groq".to_string()]).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            content
                .matches("LLM_HUB_GROQ_AUTH=github-copilot\n")
                .count(),
            1
        );
        assert_eq!(
            content
                .matches("LLM_HUB_GROQ_GITHUB_TOKEN_SOURCE=gh\n")
                .count(),
            1
        );
        assert_eq!(
            content
                .matches("LLM_HUB_GROQ_GH_PATH=/opt/bin/ghx\n")
                .count(),
            1
        );
        assert!(!content.contains("https://old"));
    }

    #[test]
    fn upsert_keeps_local_tuner_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        let tuner_lines = "LLM_HUB_GROQ_PREFILL_GUARD=on\nLLM_HUB_GROQ_TUNER_REGISTRY=~/registry.txt\nLLM_HUB_GROQ_TUNER_APPLY_COMMAND=sync --restart\nLLM_HUB_GROQ_TUNER_APPLY_TIMEOUT_MS=5000\n";
        std::fs::write(
            &path,
            format!("LLM_HUB_GROQ_BASE_URL=https://old\n{tuner_lines}"),
        )
        .unwrap();

        upsert_profile(&path, &sample_profile(), &["groq".to_string()]).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        for line in tuner_lines.lines() {
            assert_eq!(
                content.matches(&format!("{line}\n")).count(),
                1,
                "{content}"
            );
        }
        assert!(!content.contains("https://old"));
    }

    #[test]
    fn upsert_writes_keep_warm_only_when_it_is_off() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        let mut profile = sample_profile();

        profile.keep_warm = false;
        upsert_profile(&path, &profile, &["groq".to_string()]).unwrap();
        let off = std::fs::read_to_string(&path).unwrap();
        assert!(off.contains("LLM_HUB_GROQ_KEEP_WARM=false"), "{off}");

        profile.keep_warm = true;
        upsert_profile(&path, &profile, &["groq".to_string()]).unwrap();
        let on = std::fs::read_to_string(&path).unwrap();
        assert!(!on.contains("KEEP_WARM"), "{on}");
    }

    #[test]
    fn upsert_never_writes_operator_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        let mut profile = sample_profile();
        profile.managed = Some(crate::configs::ManagedCommand {
            argv: vec!["srv".into()],
            address: "127.0.0.1:1".into(),
            idle_timeout: None,
            start_timeout: std::time::Duration::from_secs(1),
        });

        upsert_profile(&path, &profile, &["groq".to_string()]).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(!content.contains("COMMAND"), "{content}");
        assert!(!content.contains("IDLE_SECS"), "{content}");
    }

    #[test]
    fn upsert_keeps_the_operator_budget_and_never_writes_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(&path, "LLM_HUB_GROQ_BUDGET_MONTHLY_USD=50\n").unwrap();
        let mut profile = sample_profile();
        profile.budget_monthly_usd = Some(75.0);

        upsert_profile(&path, &profile, &["groq".to_string()]).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            content.matches("BUDGET_MONTHLY_USD").count(),
            1,
            "{content}"
        );
        assert!(content.contains("LLM_HUB_GROQ_BUDGET_MONTHLY_USD=50\n"));
    }

    #[test]
    fn remove_profile_named_budget_keeps_the_global_budget_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(
            &path,
            "LLM_HUB_BUDGET_BASE_URL=http://127.0.0.1:1\nLLM_HUB_BUDGET_MONTHLY_USD=200\n\
             LLM_HUB_NOTIFY_BUDGET_STEPS=50,100\n",
        )
        .unwrap();

        remove_profile(&path, "budget", &[]).unwrap();
        remove_profile(&path, "notify", &[]).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.contains("LLM_HUB_BUDGET_MONTHLY_USD=200\n"),
            "{content}"
        );
        assert!(
            content.contains("LLM_HUB_NOTIFY_BUDGET_STEPS=50,100\n"),
            "{content}"
        );
        assert!(!content.contains("BASE_URL"), "{content}");
    }

    #[test]
    fn remove_profile_with_command_drops_operator_keys_too() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(
            &path,
            "LLM_HUB_GROQ_BASE_URL=https://x\nLLM_HUB_GROQ_COMMAND=srv\n",
        )
        .unwrap();

        remove_profile(&path, "groq", &[]).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(!content.contains("LLM_HUB_GROQ_"), "{content}");
    }

    #[test]
    fn remove_profile_named_like_a_global_prefix_keeps_global_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(
            &path,
            "LLM_HUB_LOCAL_BASE_URL=http://127.0.0.1:1\nLLM_HUB_LOCAL_TUNER=true\n\
             LLM_HUB_NOTIFY_BASE_URL=http://127.0.0.1:2\nLLM_HUB_NOTIFY_MUTE=source=errors\n\
             LLM_HUB_EXTENSIONS_BASE_URL=http://127.0.0.1:3\nLLM_HUB_EXTENSIONS_DISABLED=rules:*\n",
        )
        .unwrap();

        remove_profile(&path, "local", &[]).unwrap();
        remove_profile(&path, "notify", &[]).unwrap();
        remove_profile(&path, "extensions", &[]).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("LLM_HUB_LOCAL_TUNER=true\n"), "{content}");
        assert!(
            content.contains("LLM_HUB_NOTIFY_MUTE=source=errors\n"),
            "{content}"
        );
        assert!(
            content.contains("LLM_HUB_EXTENSIONS_DISABLED=rules:*\n"),
            "{content}"
        );
        assert!(!content.contains("BASE_URL"), "{content}");
    }

    #[test]
    fn set_default_fallbacks_creates_updates_and_clears() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");

        let chain = vec!["groq/llama".to_string(), "openai/gpt-4o".to_string()];
        set_default_fallbacks(&path, &chain).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("LLM_HUB_DEFAULT_FALLBACKS=groq/llama,openai/gpt-4o"));

        set_default_fallbacks(&path, &["a/b".to_string()]).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content.matches("LLM_HUB_DEFAULT_FALLBACKS=").count(), 1);
        assert!(content.contains("LLM_HUB_DEFAULT_FALLBACKS=a/b"));

        set_default_fallbacks(&path, &[]).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("LLM_HUB_DEFAULT_FALLBACKS=\n"));
    }

    #[test]
    fn remove_deletes_profile_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        let names = vec!["groq".to_string()];
        upsert_profile(&path, &sample_profile(), &names).unwrap();
        remove_profile(&path, "groq", &[]).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(!content.contains("LLM_HUB_GROQ_"));
        assert!(content.contains("LLM_HUB_PROFILES=\n") || content.contains("LLM_HUB_PROFILES="));
    }
}
