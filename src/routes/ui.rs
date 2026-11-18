use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "ui/"]
struct UiAssets;

/// Serves the embedded UI. `/` maps to index.html; unknown extension-less
/// paths (SPA routes like `/profiles`) also fall back to index.html so the
/// client-side router can take over on a direct link or page reload.
pub async fn serve(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let file = if path.is_empty() { "index.html" } else { path };
    if let Some(content) = UiAssets::get(file) {
        return asset_response(file, content);
    }
    if spa_eligible(path)
        && let Some(index) = UiAssets::get("index.html")
    {
        return asset_response("index.html", index);
    }
    (StatusCode::NOT_FOUND, "not found").into_response()
}

const HASHED_ASSET_PREFIX: &str = "assets/";
const CACHE_HASHED_ASSET: &str = "public, max-age=31536000, immutable";
const CACHE_REVALIDATE: &str = "no-cache";

/// Hashed bundle files never change under one name. Every other file (index.html) is
/// revalidated, so a rebuilt UI shows on the next load without a hard refresh.
fn cache_control(file: &str) -> &'static str {
    if file.starts_with(HASHED_ASSET_PREFIX) {
        CACHE_HASHED_ASSET
    } else {
        CACHE_REVALIDATE
    }
}

fn asset_response(file: &str, content: rust_embed::EmbeddedFile) -> Response {
    let mime = mime_guess::from_path(file).first_or_octet_stream();
    (
        [
            (header::CONTENT_TYPE, mime.as_ref().to_string()),
            (header::CACHE_CONTROL, cache_control(file).to_string()),
        ],
        content.data,
    )
        .into_response()
}

/// A path is SPA-eligible when it is not an API/proxy/health route and its
/// final segment has no extension (an asset miss like `missing.js` stays 404).
fn spa_eligible(path: &str) -> bool {
    if path.starts_with("api/") || path.starts_with("v1/") || path == "healthz" {
        return false;
    }
    let last_segment = path.rsplit('/').next().unwrap_or(path);
    !last_segment.contains('.')
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn get(path: &str) -> Response {
        serve(path.parse::<Uri>().unwrap()).await
    }

    fn header_of(response: &Response, name: header::HeaderName) -> &str {
        response.headers()[name].to_str().unwrap()
    }

    async fn body_of(response: Response) -> Vec<u8> {
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec()
    }

    fn index_bytes() -> Vec<u8> {
        UiAssets::get("index.html").unwrap().data.into_owned()
    }

    #[tokio::test]
    async fn the_root_serves_index_html_for_revalidation() {
        let response = get("/").await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(header_of(&response, header::CONTENT_TYPE), "text/html");
        assert_eq!(
            header_of(&response, header::CACHE_CONTROL),
            CACHE_REVALIDATE
        );
        assert_eq!(body_of(response).await, index_bytes());
    }

    #[tokio::test]
    async fn a_hashed_asset_is_served_as_immutable_with_its_mime_type() {
        let asset = UiAssets::iter()
            .find(|file| file.starts_with(HASHED_ASSET_PREFIX) && file.ends_with(".css"))
            .expect("the embedded UI has a hashed stylesheet");

        let response = get(&format!("/{asset}")).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(header_of(&response, header::CONTENT_TYPE), "text/css");
        assert_eq!(
            header_of(&response, header::CACHE_CONTROL),
            CACHE_HASHED_ASSET
        );
        assert_eq!(
            body_of(response).await,
            UiAssets::get(&asset).unwrap().data.into_owned()
        );
    }

    #[tokio::test]
    async fn an_spa_route_falls_back_to_index_html() {
        let response = get("/keys/some-name").await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(header_of(&response, header::CONTENT_TYPE), "text/html");
        assert_eq!(body_of(response).await, index_bytes());
    }

    #[tokio::test]
    async fn a_missing_asset_or_api_path_is_not_found() {
        for path in [
            "/assets/missing.js",
            "/api/unknown",
            "/v1/nothing",
            "/healthz",
        ] {
            let response = get(path).await;

            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
            assert_eq!(body_of(response).await, b"not found", "{path}");
        }
    }

    #[test]
    fn spa_eligible_accepts_extensionless_routes() {
        assert!(spa_eligible("profiles"));
        assert!(spa_eligible("stats"));
        assert!(spa_eligible("keys/some-name"));
    }

    #[test]
    fn index_is_revalidated_and_hashed_assets_are_immutable() {
        assert_eq!(cache_control("index.html"), CACHE_REVALIDATE);
        assert_eq!(cache_control("favicon.ico"), CACHE_REVALIDATE);
        assert_eq!(
            cache_control("assets/index-DauIu9FG.js"),
            CACHE_HASHED_ASSET
        );
    }

    #[test]
    fn spa_eligible_rejects_api_and_asset_paths() {
        assert!(!spa_eligible("api/profiles"));
        assert!(!spa_eligible("v1/models"));
        assert!(!spa_eligible("healthz"));
        assert!(!spa_eligible("assets/missing.js"));
        assert!(!spa_eligible("favicon.ico"));
    }
}
