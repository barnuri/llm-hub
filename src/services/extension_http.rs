//! The `http_request` and `http_response` extension phases, as a layer around every route.

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::dependencies::state::AppState;
use crate::ext::{Flow, HttpRequestCtx, HttpResponseCtx};
use crate::schemas::app_error::AppError;

/// Runs `http_request` before the request reaches a route, and `http_response` on what comes back.
/// A rejection answers the client without calling the route. Its response still passes
/// `http_response`.
pub async fn run_phases(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let extensions = state.extensions();
    let method = request.method().clone();
    let path = request.uri().path().to_owned();

    let mut headers = std::mem::take(request.headers_mut());
    let flow = extensions.http_request(&mut HttpRequestCtx::new(&method, &path, &mut headers));
    *request.headers_mut() = headers;
    let mut response = match flow {
        Flow::Continue => next.run(request).await,
        Flow::Reject { status, message } => AppError::Rejected(status, message).into_response(),
    };

    let original: StatusCode = response.status();
    let mut headers = std::mem::take(response.headers_mut());
    let mut ctx = HttpResponseCtx::new(&method, &path, original, &mut headers);
    extensions.http_response(&mut ctx);
    let changed = ctx.changed_status(original);
    *response.headers_mut() = headers;
    if let Some(status) = changed {
        *response.status_mut() = status;
    }
    response
}
