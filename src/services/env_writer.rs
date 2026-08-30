use std::path::Path;

use crate::configs::{ProfileConfig, env_name};

const PREFIX: &str = "LLM_HUB_";

/// Rewrites the env file with a profile added/updated. Unrelated lines,
/// comments, and ordering are preserved; the write is temp-file + atomic
/// rename in the same directory (rename across filesystems is not atomic).
pub fn upsert_profile(
    path: &Path,
    profile: &ProfileConfig,
    all_profile_names: &[String],
) -> Result<(), String> {
    let mut lines = read_lines(path)?;
    let upper = env_name(&profile.name);

    remove_profile_lines(&mut lines, &upper);
    set_var(&mut lines, "PROFILES", &all_profile_names.join(","));

    let mut new_vars: Vec<(String, String)> =
        vec![(format!("{upper}_BASE_URL"), profile.base_url.clone())];
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
    if !profile.static_models.is_empty() {
        new_vars.push((format!("{upper}_MODELS"), profile.static_models.join(",")));
    }
    for (key, value) in new_vars {
        lines.push(format!("{PREFIX}{key}={value}"));
    }

    write_atomic(path, &lines)
}

pub fn remove_profile(
    path: &Path,
    name: &str,
    remaining_profile_names: &[String],
) -> Result<(), String> {
    let mut lines = read_lines(path)?;
    remove_profile_lines(&mut lines, &env_name(name));
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

fn remove_profile_lines(lines: &mut Vec<String>, upper: &str) {
    let var_prefix = format!("{PREFIX}{upper}_");
    lines.retain(|line| !line.trim_start().starts_with(&var_prefix));
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
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let tmp = dir.join(format!(".env.tmp-{}", std::process::id()));
    let content = lines.join("\n") + "\n";
    std::fs::write(&tmp, content).map_err(|e| format!("write temp env failed: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("atomic rename failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_profile() -> ProfileConfig {
        ProfileConfig {
            name: "groq".into(),
            base_url: "https://api.groq.com/openai/v1".into(),
            api_key: "gsk-12345".into(),
            extra_headers: vec![],
            timeout_ms: None,
            enabled: true,
            static_models: vec![],
        }
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
