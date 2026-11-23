//! The desktop sink: a native popup per notification, through the OS's own command-line tool.
//!
//! macOS runs `osascript`, Linux `notify-send` and Windows a PowerShell toast. No crate is added.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use base64::Engine;
use futures_util::FutureExt;
use futures_util::future::BoxFuture;
use serde::Serialize;
use utoipa::ToSchema;

use crate::schemas::notification::{
    Notification, NotificationKind, NotificationSeverity, NotificationStatus,
};
use crate::services::local_tuner::command::{self, CommandResult};
use crate::services::store::now_ms;

/// The most title characters a popup shows.
pub const TITLE_CAP: usize = 120;
/// The most body characters a popup shows.
pub const BODY_CAP: usize = 400;
const APP_NAME: &str = "llm-hub";
const MACOS_SETTINGS_URL: &str =
    "x-apple.systempreferences:com.apple.Notifications-Settings.extension";
const WINDOWS_SETTINGS_URL: &str = "ms-settings:notifications";
// Windows shows toasts only for a registered AppId; PowerShell's own is always registered.
const POWERSHELL_APP_ID: &str =
    r"{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\WindowsPowerShell\v1.0\powershell.exe";

/// The step id that sends a test popup.
pub const STEP_SEND_TEST: &str = "send_test";
/// The step id that opens the OS notification settings.
pub const STEP_OPEN_SETTINGS: &str = "open_settings";

/// The desktop family the hub runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DesktopOs {
    #[serde(rename = "macos")]
    MacOs,
    Linux,
    Windows,
    Other,
}

impl DesktopOs {
    /// The OS this binary was built for.
    #[must_use]
    pub fn current() -> DesktopOs {
        if cfg!(target_os = "macos") {
            DesktopOs::MacOs
        } else if cfg!(target_os = "linux") {
            DesktopOs::Linux
        } else if cfg!(target_os = "windows") {
            DesktopOs::Windows
        } else {
            DesktopOs::Other
        }
    }

    /// The file name of the popup tool on `PATH`, or `None` when the OS has none.
    fn tool(self) -> Option<&'static str> {
        match self {
            DesktopOs::MacOs => Some("osascript"),
            DesktopOs::Linux => Some("notify-send"),
            DesktopOs::Windows => Some("powershell.exe"),
            DesktopOs::Other => None,
        }
    }
}

/// Runs a command for the sink, so tests can swap in a fake that spawns nothing.
pub trait Runner: Send + Sync + fmt::Debug {
    /// Runs `argv` with optional `stdin` and a `timeout`. Never fails; see [`command::run`].
    fn run<'a>(
        &'a self,
        argv: &'a [String],
        stdin: Option<Vec<u8>>,
        timeout: Duration,
    ) -> BoxFuture<'a, CommandResult>;
}

/// The real runner: [`command::run`].
#[derive(Debug, Default)]
pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run<'a>(
        &'a self,
        argv: &'a [String],
        stdin: Option<Vec<u8>>,
        timeout: Duration,
    ) -> BoxFuture<'a, CommandResult> {
        command::run(argv, stdin, timeout).boxed()
    }
}

/// Reads one environment variable. The sink takes it so tests never touch the process env.
pub type EnvReader = fn(&str) -> Option<OsString>;

fn process_env(name: &str) -> Option<OsString> {
    std::env::var_os(name)
}

/// One numbered setup step for the desktop sink.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, ToSchema)]
pub struct FixStep {
    /// A stable id. `POST /api/notifications/desktop/fix/{id}` runs it when `runnable`.
    pub id: String,
    pub text: String,
    /// A command the user can copy, when the step has one.
    pub command: Option<String>,
    /// True when the hub may run the step itself: a fixed command with no input and no sudo.
    pub runnable: bool,
}

/// A snapshot of the desktop sink for the setup banner.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, ToSchema)]
pub struct DesktopStatus {
    pub enabled: bool,
    pub os: DesktopOs,
    /// Whether the popup tool is on `PATH`. `None` when the OS has no tool.
    pub tool_found: Option<bool>,
    /// Linux only: whether the hub sees a D-Bus session or a display.
    pub session_ok: Option<bool>,
    /// When the last popup succeeded.
    pub last_ok_ms: Option<u64>,
    /// Why the last popup failed. A later success clears it.
    pub last_error: Option<String>,
    /// True once the user said they saw the test popup.
    pub confirmed: bool,
    pub steps: Vec<FixStep>,
}

/// Saves the user's confirmation, so a restart keeps it. The sink runs it off the caller's thread.
#[derive(Clone)]
pub struct ConfirmHook(Arc<dyn Fn() + Send + Sync>);

impl ConfirmHook {
    /// Wraps `persist`, which the sink calls once per [`DesktopSink::confirm`] that changes the state.
    pub fn new(persist: impl Fn() + Send + Sync + 'static) -> ConfirmHook {
        ConfirmHook(Arc::new(persist))
    }

    /// Runs the hook on the blocking pool inside a Tokio runtime, and inline outside one.
    fn run(&self) {
        let persist = Arc::clone(&self.0);
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => drop(handle.spawn_blocking(move || persist())),
            Err(_) => persist(),
        }
    }
}

impl fmt::Debug for ConfirmHook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ConfirmHook")
    }
}

#[derive(Debug, Default)]
struct SendState {
    last_ok_ms: Option<u64>,
    last_error: Option<String>,
    confirmed: bool,
}

/// Shows notifications as desktop popups and tracks whether they work.
#[derive(Debug)]
pub struct DesktopSink {
    enabled: bool,
    os: DesktopOs,
    runner: Arc<dyn Runner>,
    timeout: Duration,
    env: EnvReader,
    state: Mutex<SendState>,
    confirm_hook: Option<ConfirmHook>,
}

impl DesktopSink {
    /// A sink for this OS that runs real commands and reads the process env.
    #[must_use]
    pub fn new(enabled: bool, timeout: Duration) -> DesktopSink {
        DesktopSink::with_parts(
            enabled,
            DesktopOs::current(),
            Arc::new(SystemRunner),
            timeout,
            process_env,
        )
    }

    /// A sink with every dependency given, for tests and other hosts.
    #[must_use]
    pub fn with_parts(
        enabled: bool,
        os: DesktopOs,
        runner: Arc<dyn Runner>,
        timeout: Duration,
        env: EnvReader,
    ) -> DesktopSink {
        DesktopSink {
            enabled,
            os,
            runner,
            timeout,
            env,
            state: Mutex::new(SendState::default()),
            confirm_hook: None,
        }
    }

    /// Starts the sink as `confirmed`, from a saved confirmation, and saves later ones with `hook`.
    #[must_use]
    pub fn with_confirmation(mut self, confirmed: bool, hook: Option<ConfirmHook>) -> DesktopSink {
        self.lock().confirmed = confirmed;
        self.confirm_hook = hook;
        self
    }

    /// True when the dispatcher should send to this sink.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Shows `notification` as a popup and records the outcome in [`DesktopSink::status`].
    ///
    /// # Errors
    /// Returns a message when the OS has no popup tool or the tool fails.
    pub async fn send(&self, notification: &Notification) -> Result<(), String> {
        let result = match desktop_argv(self.os, &notification.title, &notification.body) {
            Some(argv) => {
                let outcome = self.runner.run(&argv, None, self.timeout).await;
                if outcome.ok {
                    Ok(())
                } else {
                    Err(outcome.tail)
                }
            }
            None => Err("desktop notifications are not supported on this OS".to_string()),
        };
        let mut state = self.lock();
        match &result {
            Ok(()) => {
                state.last_ok_ms = Some(now_ms());
                state.last_error = None;
            }
            Err(error) => state.last_error = Some(error.clone()),
        }
        result
    }

    /// Sends a fixed test popup.
    ///
    /// # Errors
    /// Returns the send error.
    pub async fn send_test(&self) -> Result<(), String> {
        self.send(&test_notification()).await
    }

    /// Runs the runnable fix step `step_id` for this OS. Only the fixed allowlist runs.
    ///
    /// # Errors
    /// Returns a message for an unknown or copy-only step, or when the step fails.
    pub async fn run_fix(&self, step_id: &str) -> Result<(), String> {
        let Some(step) = fix_steps(self.os)
            .into_iter()
            .find(|step| step.id == step_id)
        else {
            return Err(format!("unknown fix step {step_id}"));
        };
        if !step.runnable {
            return Err(format!("fix step {step_id} is copy-only"));
        }
        if step.id == STEP_SEND_TEST {
            return self.send_test().await;
        }
        let Some(argv) = fix_argv(self.os, &step.id) else {
            return Err(format!("fix step {step_id} has no command"));
        };
        let outcome = self.runner.run(&argv, None, self.timeout).await;
        if outcome.ok {
            Ok(())
        } else {
            Err(outcome.tail)
        }
    }

    /// Records that the user saw the test popup. The first confirmation also runs the confirm hook.
    pub fn confirm(&self) {
        let was_confirmed = std::mem::replace(&mut self.lock().confirmed, true);
        if !was_confirmed && let Some(hook) = &self.confirm_hook {
            hook.run();
        }
    }

    /// The current state, with the tool and session checked now. The check reads only env
    /// and file metadata; it spawns nothing.
    #[must_use]
    pub fn status(&self) -> DesktopStatus {
        let tool_found = self
            .os
            .tool()
            .map(|tool| (self.env)("PATH").is_some_and(|path| find_on_path(tool, &path)));
        let session_ok = (self.os == DesktopOs::Linux).then(|| linux_session_ok(self.env));
        let state = self.lock();
        DesktopStatus {
            enabled: self.enabled,
            os: self.os,
            tool_found,
            session_ok,
            last_ok_ms: state.last_ok_ms,
            last_error: state.last_error.clone(),
            confirmed: state.confirmed,
            steps: fix_steps(self.os),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SendState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The command that shows one popup on `os`, or `None` when the OS has no popup tool.
/// Title and body are capped at [`TITLE_CAP`] and [`BODY_CAP`] characters.
#[must_use]
pub fn desktop_argv(os: DesktopOs, title: &str, body: &str) -> Option<Vec<String>> {
    let title = cap(title, TITLE_CAP);
    let body = cap(body, BODY_CAP);
    match os {
        DesktopOs::MacOs => {
            let script = format!(
                "display notification \"{}\" with title \"{APP_NAME}\" subtitle \"{}\"",
                applescript_escape(&body),
                applescript_escape(&title)
            );
            Some(vec!["osascript".into(), "-e".into(), script])
        }
        // `--` stops a title that starts with `-` from being read as an option.
        DesktopOs::Linux => Some(vec![
            "notify-send".into(),
            format!("--app-name={APP_NAME}"),
            "--".into(),
            title,
            body,
        ]),
        DesktopOs::Windows => Some(vec![
            "powershell".into(),
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            windows_toast_script(&title, &body),
        ]),
        DesktopOs::Other => None,
    }
}

/// Keeps the first `max` characters, ending with `…` when it cut any.
fn cap(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut capped: String = text.chars().take(max.saturating_sub(1)).collect();
    capped.push('…');
    capped
}

/// Escapes text for an `AppleScript` string literal. Line breaks and other control
/// characters become spaces, since a popup shows one line.
fn applescript_escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            ch if ch.is_control() => escaped.push(' '),
            ch => escaped.push(ch),
        }
    }
    escaped
}

fn xml_escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            ch => escaped.push(ch),
        }
    }
    escaped
}

/// The PowerShell toast script. The hub XML-escapes the text and passes the toast XML as
/// base64, so no title or body character can end a PowerShell string.
fn windows_toast_script(title: &str, body: &str) -> String {
    let xml = format!(
        "<toast><visual><binding template=\"ToastGeneric\"><text>{APP_NAME}: {}</text><text>{}</text></binding></visual></toast>",
        xml_escape(title),
        xml_escape(body)
    );
    let encoded = base64::engine::general_purpose::STANDARD.encode(xml);
    format!(
        "$ErrorActionPreference='Stop';\
         [Windows.UI.Notifications.ToastNotificationManager,Windows.UI.Notifications,ContentType=WindowsRuntime]>$null;\
         [Windows.Data.Xml.Dom.XmlDocument,Windows.Data.Xml.Dom.XmlDocument,ContentType=WindowsRuntime]>$null;\
         $x=New-Object Windows.Data.Xml.Dom.XmlDocument;\
         $x.LoadXml([Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{encoded}')));\
         [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('{POWERSHELL_APP_ID}').Show((New-Object Windows.UI.Notifications.ToastNotification $x))"
    )
}

/// True when `program` is a file in one of the `path` directories.
fn find_on_path(program: &str, path: &OsStr) -> bool {
    std::env::split_paths(path).any(|dir| dir.join(program).is_file())
}

/// True when a `notify-send` started by the hub can reach the desktop's notification daemon.
fn linux_session_ok(env: EnvReader) -> bool {
    ["DBUS_SESSION_BUS_ADDRESS", "WAYLAND_DISPLAY", "DISPLAY"]
        .into_iter()
        .any(|name| env(name).is_some_and(|value| !value.is_empty()))
}

fn step(id: &str, text: &str, command: Option<&str>, runnable: bool) -> FixStep {
    FixStep {
        id: id.to_string(),
        text: text.to_string(),
        command: command.map(str::to_string),
        runnable,
    }
}

fn send_test_step() -> FixStep {
    step(
        STEP_SEND_TEST,
        "Send a test popup, then press \"I saw it\".",
        None,
        true,
    )
}

/// The setup steps for `os`, in the order the banner numbers them.
#[must_use]
pub fn fix_steps(os: DesktopOs) -> Vec<FixStep> {
    match os {
        DesktopOs::MacOs => vec![
            step(
                STEP_OPEN_SETTINGS,
                "Open System Settings > Notifications, select \"Script Editor\" and allow notifications.",
                Some(&format!("open \"{MACOS_SETTINGS_URL}\"")),
                true,
            ),
            step(
                "focus_off",
                "Turn off Focus (Do Not Disturb) in Control Center.",
                None,
                false,
            ),
            send_test_step(),
        ],
        DesktopOs::Linux => vec![
            step(
                "install_libnotify_apt",
                "Install libnotify for notify-send (Debian, Ubuntu).",
                Some("sudo apt install libnotify-bin"),
                false,
            ),
            step(
                "install_libnotify_dnf",
                "Install libnotify for notify-send (Fedora, RHEL).",
                Some("sudo dnf install libnotify"),
                false,
            ),
            step(
                "desktop_session",
                "Run the hub in your desktop session so it sees DBUS_SESSION_BUS_ADDRESS. For a systemd user service, import the session env.",
                Some(
                    "systemctl --user import-environment DBUS_SESSION_BUS_ADDRESS DISPLAY WAYLAND_DISPLAY",
                ),
                false,
            ),
            send_test_step(),
        ],
        DesktopOs::Windows => vec![
            step(
                STEP_OPEN_SETTINGS,
                "Turn on notifications in Settings > System > Notifications, and allow Windows PowerShell.",
                Some(&format!("start {WINDOWS_SETTINGS_URL}")),
                true,
            ),
            step("dnd_off", "Turn off Do not disturb.", None, false),
            send_test_step(),
        ],
        DesktopOs::Other => vec![step(
            "unsupported",
            "Desktop popups are not supported on this OS. Use LLM_HUB_NOTIFY_WEBHOOK_URL or LLM_HUB_NOTIFY_COMMAND.",
            None,
            false,
        )],
    }
}

/// The fixed command of a runnable step other than the test popup.
fn fix_argv(os: DesktopOs, step_id: &str) -> Option<Vec<String>> {
    match (os, step_id) {
        (DesktopOs::MacOs, STEP_OPEN_SETTINGS) => {
            Some(vec!["open".into(), MACOS_SETTINGS_URL.into()])
        }
        (DesktopOs::Windows, STEP_OPEN_SETTINGS) => Some(vec![
            "cmd".into(),
            "/c".into(),
            "start".into(),
            WINDOWS_SETTINGS_URL.into(),
        ]),
        _ => None,
    }
}

/// The fixed notification the test popup shows.
#[must_use]
pub fn test_notification() -> Notification {
    let now = now_ms();
    Notification {
        id: "desktop-test".to_string(),
        kind: NotificationKind::Custom,
        source: "api".to_string(),
        severity: NotificationSeverity::Info,
        title: "Test notification".to_string(),
        body: "Desktop notifications from llm-hub work.".to_string(),
        profile: None,
        model: None,
        rule: None,
        diff: None,
        actions: Notification::default_actions(NotificationKind::Custom),
        status: NotificationStatus::Pending,
        created_ms: now,
        updated_ms: now,
        result: None,
        muted: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records every argv and answers with one scripted result.
    #[derive(Debug)]
    struct FakeRunner {
        result: CommandResult,
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl FakeRunner {
        fn new(ok: bool, tail: &str) -> Arc<FakeRunner> {
            Arc::new(FakeRunner {
                result: CommandResult {
                    ok,
                    tail: tail.to_string(),
                },
                calls: Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Runner for FakeRunner {
        fn run<'a>(
            &'a self,
            argv: &'a [String],
            stdin: Option<Vec<u8>>,
            _timeout: Duration,
        ) -> BoxFuture<'a, CommandResult> {
            assert!(stdin.is_none());
            self.calls.lock().unwrap().push(argv.to_vec());
            std::future::ready(self.result.clone()).boxed()
        }
    }

    fn no_env(_: &str) -> Option<OsString> {
        None
    }

    fn sink(os: DesktopOs, runner: &Arc<FakeRunner>, env: EnvReader) -> DesktopSink {
        let runner: Arc<dyn Runner> = Arc::clone(runner) as Arc<dyn Runner>;
        DesktopSink::with_parts(true, os, runner, Duration::from_secs(5), env)
    }

    fn notification(title: &str, body: &str) -> Notification {
        Notification {
            title: title.to_string(),
            body: body.to_string(),
            ..test_notification()
        }
    }

    #[test]
    fn current_os_matches_the_build_target() {
        let expected = if cfg!(target_os = "macos") {
            DesktopOs::MacOs
        } else if cfg!(target_os = "linux") {
            DesktopOs::Linux
        } else if cfg!(target_os = "windows") {
            DesktopOs::Windows
        } else {
            DesktopOs::Other
        };
        assert_eq!(DesktopOs::current(), expected);
        assert_eq!(serde_json::to_value(DesktopOs::MacOs).unwrap(), "macos");
    }

    #[test]
    fn applescript_escape_handles_quotes_backslashes_newlines_and_unicode() {
        assert_eq!(
            applescript_escape("say \"hi\" \\ there\nnext\r\tend ünï 🚀"),
            "say \\\"hi\\\" \\\\ there next  end ünï 🚀"
        );
    }

    #[test]
    fn macos_argv_wraps_escaped_text_in_display_notification() {
        let argv = desktop_argv(DesktopOs::MacOs, "Gem \"ma\"", "a\\b\nc").unwrap();

        assert_eq!(
            argv,
            vec![
                "osascript".to_string(),
                "-e".to_string(),
                "display notification \"a\\\\b c\" with title \"llm-hub\" subtitle \"Gem \\\"ma\\\"\""
                    .to_string(),
            ]
        );
    }

    #[test]
    fn linux_argv_passes_title_and_body_as_separate_arguments() {
        let argv = desktop_argv(DesktopOs::Linux, "-t title", "line 1\nline \"2\"").unwrap();

        assert_eq!(
            argv,
            vec![
                "notify-send",
                "--app-name=llm-hub",
                "--",
                "-t title",
                "line 1\nline \"2\""
            ]
        );
    }

    #[test]
    fn windows_argv_passes_xml_escaped_text_as_base64() {
        let argv = desktop_argv(DesktopOs::Windows, "a<b>&'", "it's \"x\" ü").unwrap();

        assert_eq!(
            argv[..4],
            ["powershell", "-NoProfile", "-NonInteractive", "-Command"]
        );
        let script = &argv[4];
        assert!(!script.contains("it's"), "{script}");
        assert!(script.contains(POWERSHELL_APP_ID), "{script}");
        let encoded = script
            .split("FromBase64String('")
            .nth(1)
            .and_then(|rest| rest.split('\'').next())
            .unwrap();
        let xml = String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            xml,
            "<toast><visual><binding template=\"ToastGeneric\"><text>llm-hub: a&lt;b&gt;&amp;&apos;</text><text>it&apos;s &quot;x&quot; ü</text></binding></visual></toast>"
        );
    }

    #[test]
    fn other_os_has_no_argv() {
        assert_eq!(desktop_argv(DesktopOs::Other, "t", "b"), None);
    }

    #[test]
    fn argv_caps_title_and_body_by_characters() {
        let title = "é".repeat(TITLE_CAP + 10);
        let body = "ü".repeat(BODY_CAP + 10);

        let argv = desktop_argv(DesktopOs::Linux, &title, &body).unwrap();

        assert_eq!(argv[3].chars().count(), TITLE_CAP);
        assert!(argv[3].ends_with("é…"));
        assert_eq!(argv[4].chars().count(), BODY_CAP);
        assert!(argv[4].ends_with('…'));
        assert_eq!(cap("short", TITLE_CAP), "short");
        assert_eq!(
            cap(&"x".repeat(TITLE_CAP), TITLE_CAP),
            "x".repeat(TITLE_CAP)
        );
    }

    #[tokio::test]
    async fn send_success_records_last_ok_and_clears_error() {
        let runner = FakeRunner::new(false, "boom");
        let failing = sink(DesktopOs::Linux, &runner, no_env);
        assert_eq!(
            failing.send(&notification("t", "b")).await,
            Err("boom".into())
        );
        assert_eq!(failing.status().last_error.as_deref(), Some("boom"));
        assert_eq!(failing.status().last_ok_ms, None);

        let runner = FakeRunner::new(true, "");
        let sink = sink(DesktopOs::Linux, &runner, no_env);
        sink.lock().last_error = Some("old".into());
        sink.send(&notification("t", "b")).await.unwrap();

        let status = sink.status();
        assert!(status.last_ok_ms.is_some());
        assert_eq!(status.last_error, None);
        assert_eq!(
            runner.calls(),
            vec![desktop_argv(DesktopOs::Linux, "t", "b").unwrap()]
        );
    }

    #[tokio::test]
    async fn send_on_unsupported_os_fails_without_running() {
        let runner = FakeRunner::new(true, "");
        let sink = sink(DesktopOs::Other, &runner, no_env);

        let error = sink.send(&notification("t", "b")).await.unwrap_err();

        assert!(error.contains("not supported"), "{error}");
        assert_eq!(sink.status().last_error, Some(error));
        assert!(runner.calls().is_empty());
    }

    #[tokio::test]
    async fn run_fix_send_test_sends_the_test_popup() {
        let runner = FakeRunner::new(true, "");
        let sink = sink(DesktopOs::MacOs, &runner, no_env);

        sink.run_fix(STEP_SEND_TEST).await.unwrap();

        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0][2].contains("Test notification"), "{:?}", calls[0]);
    }

    #[tokio::test]
    async fn run_fix_open_settings_runs_the_fixed_command_per_os() {
        let runner = FakeRunner::new(true, "");
        sink(DesktopOs::MacOs, &runner, no_env)
            .run_fix(STEP_OPEN_SETTINGS)
            .await
            .unwrap();
        sink(DesktopOs::Windows, &runner, no_env)
            .run_fix(STEP_OPEN_SETTINGS)
            .await
            .unwrap();

        assert_eq!(
            runner.calls(),
            vec![
                vec!["open".to_string(), MACOS_SETTINGS_URL.to_string()],
                vec![
                    "cmd".to_string(),
                    "/c".to_string(),
                    "start".to_string(),
                    WINDOWS_SETTINGS_URL.to_string()
                ],
            ]
        );
    }

    #[tokio::test]
    async fn run_fix_reports_a_failing_command() {
        let runner = FakeRunner::new(false, "open failed");
        let sink = sink(DesktopOs::MacOs, &runner, no_env);

        assert_eq!(
            sink.run_fix(STEP_OPEN_SETTINGS).await,
            Err("open failed".into())
        );
    }

    #[tokio::test]
    async fn run_fix_rejects_unknown_and_copy_only_steps() {
        let runner = FakeRunner::new(true, "");
        let linux = sink(DesktopOs::Linux, &runner, no_env);
        let mac = sink(DesktopOs::MacOs, &runner, no_env);

        let unknown = linux.run_fix("rm -rf /").await.unwrap_err();
        let copy_only = linux.run_fix("install_libnotify_apt").await.unwrap_err();
        let no_pane = linux.run_fix(STEP_OPEN_SETTINGS).await.unwrap_err();
        let focus = mac.run_fix("focus_off").await.unwrap_err();

        assert!(unknown.contains("unknown"), "{unknown}");
        assert!(copy_only.contains("copy-only"), "{copy_only}");
        assert!(no_pane.contains("unknown"), "{no_pane}");
        assert!(focus.contains("copy-only"), "{focus}");
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn every_runnable_step_has_an_action() {
        for os in [
            DesktopOs::MacOs,
            DesktopOs::Linux,
            DesktopOs::Windows,
            DesktopOs::Other,
        ] {
            let steps = fix_steps(os);
            assert!(!steps.is_empty());
            for step in steps.iter().filter(|step| step.runnable) {
                assert!(
                    step.id == STEP_SEND_TEST || fix_argv(os, &step.id).is_some(),
                    "{os:?} {}",
                    step.id
                );
                let command = step.command.as_deref().unwrap_or_default();
                assert!(!command.contains("sudo"), "{command}");
            }
        }
        assert_eq!(fix_argv(DesktopOs::Linux, STEP_OPEN_SETTINGS), None);
    }

    #[test]
    fn linux_session_needs_dbus_or_a_display() {
        fn dbus(name: &str) -> Option<OsString> {
            (name == "DBUS_SESSION_BUS_ADDRESS").then(|| "unix:path=/run/user/1/bus".into())
        }
        fn wayland(name: &str) -> Option<OsString> {
            (name == "WAYLAND_DISPLAY").then(|| "wayland-0".into())
        }
        let empty: EnvReader = |_| Some(OsString::new());

        assert!(linux_session_ok(dbus));
        assert!(linux_session_ok(wayland));
        assert!(!linux_session_ok(empty));
        assert!(!linux_session_ok(no_env));
    }

    #[test]
    fn find_on_path_checks_each_directory_for_a_file() {
        let empty = tempfile::tempdir().unwrap();
        let tools = tempfile::tempdir().unwrap();
        std::fs::write(tools.path().join("notify-send"), "").unwrap();
        std::fs::create_dir(tools.path().join("osascript")).unwrap();
        let path = std::env::join_paths([empty.path(), tools.path()]).unwrap();

        assert!(find_on_path("notify-send", &path));
        assert!(!find_on_path("osascript", &path));
        assert!(!find_on_path("powershell.exe", &path));
    }

    #[tokio::test]
    async fn status_reports_os_tool_session_steps_and_confirm() {
        fn linux_env(name: &str) -> Option<OsString> {
            match name {
                "PATH" => Some("/nonexistent-llm-hub-test-dir".into()),
                "DISPLAY" => Some(":0".into()),
                _ => None,
            }
        }
        let runner = FakeRunner::new(true, "");
        let linux = sink(DesktopOs::Linux, &runner, linux_env);
        let other = sink(DesktopOs::Other, &runner, no_env);
        let mac = sink(DesktopOs::MacOs, &runner, no_env);

        let status = linux.status();
        assert!(status.enabled && linux.enabled());
        assert_eq!(status.os, DesktopOs::Linux);
        assert_eq!(status.tool_found, Some(false));
        assert_eq!(status.session_ok, Some(true));
        assert!(!status.confirmed);
        assert_eq!(status.steps, fix_steps(DesktopOs::Linux));

        linux.confirm();
        assert!(linux.status().confirmed);

        assert_eq!(other.status().tool_found, None);
        assert_eq!(mac.status().tool_found, Some(false));
        assert_eq!(mac.status().session_ok, None);

        let value = serde_json::to_value(linux.status()).unwrap();
        assert_eq!(value["os"], "linux");
        assert_eq!(value["steps"][0]["runnable"], false);
    }

    #[tokio::test]
    async fn confirm_runs_the_hook_once_off_the_caller_thread() {
        let (tx, rx) = std::sync::mpsc::channel();
        let caller = std::thread::current().id();
        let hook = ConfirmHook::new(move || {
            tx.send(std::thread::current().id()).unwrap();
        });
        let runner = FakeRunner::new(true, "");
        let sink = sink(DesktopOs::MacOs, &runner, no_env).with_confirmation(false, Some(hook));
        assert!(!sink.status().confirmed);

        sink.confirm();
        sink.confirm();

        assert!(sink.status().confirmed);
        let ran_on = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_ne!(ran_on, caller);
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
        assert_eq!(format!("{:?}", sink.confirm_hook), "Some(ConfirmHook)");
    }

    #[test]
    fn confirm_outside_a_runtime_runs_the_hook_inline() {
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = Arc::clone(&count);
        let hook = ConfirmHook::new(move || {
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        let runner = FakeRunner::new(true, "");
        let sink = sink(DesktopOs::Linux, &runner, no_env).with_confirmation(false, Some(hook));

        sink.confirm();

        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn a_saved_confirmation_starts_the_sink_confirmed_without_the_hook() {
        let runner = FakeRunner::new(true, "");
        let sink = sink(DesktopOs::Linux, &runner, no_env).with_confirmation(true, None);

        assert!(sink.status().confirmed);
        sink.confirm();
        assert!(sink.status().confirmed);
    }

    #[test]
    fn new_uses_this_os() {
        let sink = DesktopSink::new(false, Duration::from_secs(1));

        assert!(!sink.enabled());
        assert_eq!(sink.status().os, DesktopOs::current());
    }

    #[tokio::test]
    async fn system_runner_runs_through_command_run() {
        let result = SystemRunner.run(&[], None, Duration::from_secs(1)).await;

        assert!(!result.ok);
        assert_eq!(result.tail, "no command is configured");
    }
}
