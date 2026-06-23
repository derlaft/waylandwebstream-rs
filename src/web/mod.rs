// Serves the Vite+Svelte client embedded in the binary at build time (see
// `build.rs`, which runs `npm run build` in `web/` into `web/dist/`).

use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "web/dist/"]
struct WebAssets;

/// `GET /` — the app shell.
pub async fn serve_index() -> Response {
    serve_path("index.html")
}

/// Fallback for any path not matched by another route (`/assets/*.js`,
/// `/assets/*.css`, etc).
pub async fn serve_asset(uri: Uri) -> Response {
    serve_path(uri.path().trim_start_matches('/'))
}

fn serve_path(path: &str) -> Response {
    match WebAssets::get(path) {
        Some(file) => {
            let mime = file.metadata.mimetype();
            ([(header::CONTENT_TYPE, mime)], file.data.into_owned()).into_response()
        }
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}
