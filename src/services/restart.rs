use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use axum::body::Body;
use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use futures_util::StreamExt;
use tokio::sync::Notify;

use crate::services::instance;

/// Delay before firing the shutdown notify so the HTTP response that
/// requested the restart can flush to the client first.
const RESPONSE_FLUSH_DELAY: Duration = Duration::from_millis(300);

static RESTART_REQUESTED: AtomicBool = AtomicBool::new(false);
/// Set once a successor listens. The old process then exits without a respawn.
static HANDED_OVER: AtomicBool = AtomicBool::new(false);
/// Inference requests in flight, including streamed bodies not yet sent in full.
static ACTIVE_INFERENCE: AtomicUsize = AtomicUsize::new(0);

/// Holds one in-flight inference request. The count drops when the guard drops.
pub struct InFlight(());

impl InFlight {
    /// Counts one more in-flight request until the returned guard drops.
    #[must_use]
    pub fn enter() -> Self {
        ACTIVE_INFERENCE.fetch_add(1, Ordering::SeqCst);
        Self(())
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        ACTIVE_INFERENCE.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Inference requests in flight now.
#[must_use]
pub fn active_inference() -> usize {
    ACTIVE_INFERENCE.load(Ordering::SeqCst)
}

/// Counts each `/v1/` request until its response body ends, so a restart waits for open chats.
pub async fn track_inference(request: Request, next: Next) -> Response {
    if !request.uri().path().starts_with("/v1/") {
        return next.run(request).await;
    }
    let guard = InFlight::enter();
    let (parts, body) = next.run(request).await.into_parts();
    let stream = body.into_data_stream().map(move |chunk| {
        let _held = &guard;
        chunk
    });
    Response::from_parts(parts, Body::from_stream(stream))
}

/// True when a supervisor (launchd / systemd / Task Scheduler) will bring the
/// process back after exit — `llm-hub service install` sets `LLM_HUB_SERVICE=1`.
pub fn is_supervised() -> bool {
    service_flag_set(std::env::var("LLM_HUB_SERVICE").ok().as_deref())
}

fn service_flag_set(value: Option<&str>) -> bool {
    value == Some("1")
}

/// Requests a graceful restart: flags the post-drain respawn and fires the
/// shutdown notify shortly after, letting in-flight responses complete.
pub fn request_restart(restart_notify: Arc<Notify>) {
    flag_restart(&RESTART_REQUESTED, restart_notify, hand_over());
}

/// Starts the successor, then stops this process once it listens. A failed handover keeps this
/// process serving and clears the request.
fn flag_restart(
    requested: &'static AtomicBool,
    restart_notify: Arc<Notify>,
    handover: impl std::future::Future<Output = Result<u32, String>> + Send + 'static,
) {
    requested.store(true, Ordering::SeqCst);
    tokio::spawn(async move {
        tokio::time::sleep(RESPONSE_FLUSH_DELAY).await;
        match handover.await {
            Ok(successor) => {
                HANDED_OVER.store(true, Ordering::SeqCst);
                tracing::info!(successor, "handed over to the new llm-hub process");
                restart_notify.notify_one();
            }
            Err(error) => {
                requested.store(false, Ordering::SeqCst);
                tracing::error!(%error, "restart cancelled: this process keeps serving");
            }
        }
    });
}

/// Starts the successor of this process with the same executable and arguments.
async fn hand_over() -> Result<u32, String> {
    let bind = instance::bind().ok_or("the hub has not started serving")?;
    let exe = crate::utils::exe::current_exe_resolved()
        .map_err(|e| format!("cannot resolve current exe: {e}"))?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    instance::spawn_successor(bind, &exe, args).await
}

/// After the server drains: respawn the current executable (same args) when
/// no supervisor will do it for us. Returns whether a child was spawned.
///
/// # Errors
/// Fails when the current executable path cannot be resolved or the spawn fails.
pub fn respawn_if_requested() -> Result<bool, String> {
    respawn(
        RESTART_REQUESTED.load(Ordering::SeqCst) && !HANDED_OVER.load(Ordering::SeqCst),
        is_supervised(),
        crate::utils::exe::current_exe_resolved,
        spawn_detached,
    )
}

fn respawn(
    requested: bool,
    supervised: bool,
    resolve_exe: impl FnOnce() -> std::io::Result<PathBuf>,
    spawn: impl FnOnce(&Path, Vec<String>) -> std::io::Result<()>,
) -> Result<bool, String> {
    if !requested || supervised {
        return Ok(false);
    }
    let exe = resolve_exe().map_err(|e| format!("cannot resolve current exe: {e}"))?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    spawn(&exe, args).map_err(|e| format!("respawn failed: {e}"))?;
    Ok(true)
}

fn spawn_detached(exe: &Path, args: Vec<String>) -> std::io::Result<()> {
    Command::new(exe).args(args).spawn().map(drop)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    fn unexpected_resolve() -> std::io::Result<PathBuf> {
        panic!("the exe must not be resolved")
    }

    fn unexpected_spawn(_: &Path, _: Vec<String>) -> std::io::Result<()> {
        panic!("nothing must be spawned")
    }

    #[test]
    fn respawn_without_request_is_noop() {
        assert_eq!(respawn_if_requested(), Ok(false));
    }

    #[test]
    fn only_a_service_flag_of_one_means_supervised() {
        assert!(service_flag_set(Some("1")));
        assert!(!service_flag_set(Some("0")));
        assert!(!service_flag_set(Some("")));
        assert!(!service_flag_set(None));
    }

    #[test]
    fn an_unrequested_restart_spawns_nothing() {
        let result = respawn(false, false, unexpected_resolve, unexpected_spawn);

        assert_eq!(result, Ok(false));
    }

    #[test]
    fn a_supervised_restart_leaves_the_respawn_to_the_supervisor() {
        let result = respawn(true, true, unexpected_resolve, unexpected_spawn);

        assert_eq!(result, Ok(false));
    }

    #[test]
    fn a_requested_restart_respawns_the_resolved_exe_with_the_current_args() {
        let spawned = RefCell::new(None);

        let result = respawn(
            true,
            false,
            || Ok(PathBuf::from("/opt/llm-hub/llm-hub")),
            |exe, args| {
                spawned.replace(Some((exe.to_path_buf(), args)));
                Ok(())
            },
        );

        assert_eq!(result, Ok(true));
        let expected_args: Vec<String> = std::env::args().skip(1).collect();
        assert_eq!(
            spawned.into_inner(),
            Some((PathBuf::from("/opt/llm-hub/llm-hub"), expected_args))
        );
    }

    #[test]
    fn an_unresolvable_exe_is_an_error() {
        let result = respawn(
            true,
            false,
            || Err(std::io::Error::other("no exe")),
            unexpected_spawn,
        );

        assert_eq!(
            result,
            Err("cannot resolve current exe: no exe".to_string())
        );
    }

    #[test]
    fn a_failed_spawn_is_an_error() {
        let result = respawn(
            true,
            false,
            || Ok(PathBuf::from("/opt/llm-hub/llm-hub")),
            |_, _| Err(std::io::Error::other("spawn refused")),
        );

        assert_eq!(result, Err("respawn failed: spawn refused".to_string()));
    }

    #[test]
    fn spawning_a_missing_exe_fails() {
        let dir = tempfile::tempdir().unwrap();

        let result = spawn_detached(&dir.path().join("missing-llm-hub"), Vec::new());

        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn a_completed_handover_flags_the_restart_and_notifies_after_the_flush_delay() {
        let requested: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
        let notify = Arc::new(Notify::new());
        let started = tokio::time::Instant::now();

        flag_restart(requested, Arc::clone(&notify), async { Ok(4242) });
        let notified = tokio::time::timeout(Duration::from_secs(10), notify.notified()).await;

        assert!(notified.is_ok());
        assert!(requested.load(Ordering::SeqCst));
        assert!(HANDED_OVER.load(Ordering::SeqCst));
        assert!(started.elapsed() >= RESPONSE_FLUSH_DELAY);
    }

    #[tokio::test]
    async fn a_failed_handover_keeps_serving_and_clears_the_request() {
        let requested: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
        let notify = Arc::new(Notify::new());

        flag_restart(requested, Arc::clone(&notify), async {
            Err("no successor".to_owned())
        });
        let notified = tokio::time::timeout(Duration::from_millis(800), notify.notified()).await;

        assert!(
            notified.is_err(),
            "a failed handover must not stop this process"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!requested.load(Ordering::SeqCst));
    }

    #[test]
    fn inference_guards_count_until_dropped() {
        let before = active_inference();
        let first = InFlight::enter();
        let second = InFlight::enter();
        assert_eq!(active_inference(), before + 2);
        drop(first);
        assert_eq!(active_inference(), before + 1);
        drop(second);
        assert_eq!(active_inference(), before);
    }

    /// Serves `app` on a loopback port and returns its base URL.
    async fn serve_app(app: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("address"));
        tokio::spawn(async move { axum::serve(listener, app).await });
        url
    }

    #[tokio::test]
    async fn a_streamed_inference_body_holds_its_count_until_it_ends() {
        use std::sync::Mutex;

        use axum::Router;
        use axum::routing::post;
        use tokio::sync::mpsc;

        let (tx, rx) = mpsc::channel::<bytes::Bytes>(4);
        let rx = Arc::new(Mutex::new(Some(rx)));
        let app = Router::new()
            .route(
                "/v1/chat",
                post(move || {
                    let rx = rx.lock().expect("lock").take().expect("one stream");
                    async move {
                        let chunks = futures_util::stream::unfold(rx, |mut rx| async move {
                            rx.recv().await.map(|b| (Ok::<_, std::io::Error>(b), rx))
                        });
                        Body::from_stream(chunks)
                    }
                }),
            )
            .layer(axum::middleware::from_fn(track_inference));
        let base = serve_app(app).await;
        let before = active_inference();

        let client = reqwest::Client::new();
        let mut response = client
            .post(format!("{base}/v1/chat"))
            .send()
            .await
            .expect("send");
        tx.send(bytes::Bytes::from_static(b"first"))
            .await
            .expect("chunk");
        assert_eq!(
            response.chunk().await.expect("chunk").as_deref(),
            Some(&b"first"[..])
        );
        assert_eq!(active_inference(), before + 1, "the open stream must count");

        drop(tx);
        assert!(response.chunk().await.expect("end").is_none());
        let mut waited = 0;
        while active_inference() > before && waited < 50 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            waited += 1;
        }
        assert_eq!(
            active_inference(),
            before,
            "the count must drop when the body ends"
        );
    }

    #[tokio::test]
    async fn other_paths_are_not_counted() {
        use axum::Router;
        use axum::routing::post;

        let app = Router::new()
            .route("/api/stats", post(|| async { "ok" }))
            .layer(axum::middleware::from_fn(track_inference));
        let base = serve_app(app).await;
        let before = active_inference();

        let client = reqwest::Client::new();
        let response = client
            .post(format!("{base}/api/stats"))
            .send()
            .await
            .expect("send");
        assert_eq!(response.text().await.expect("body"), "ok");
        assert_eq!(active_inference(), before);
    }
}
