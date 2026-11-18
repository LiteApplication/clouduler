//! Serves the React build from `ui/dist` with an SPA fallback so client-side routes survive a hard refresh.
//!
//! `CL_API_UI_DIR` is set by `build.rs`: the real `ui/dist` when built, else a placeholder under `OUT_DIR`, so this crate always compiles.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "$CL_API_UI_DIR"]
struct Assets;

/// Falls back to `index.html` for anything not found so client-side routes resolve to the app shell.
pub async fn serve(request: Request) -> Response {
    let path = request.uri().path().trim_start_matches('/');
    serve_path(path).unwrap_or_else(|| serve_path("index.html").unwrap_or_else(not_found))
}

fn serve_path(path: &str) -> Option<Response> {
    let file = Assets::get(path)?;
    let mime = file.metadata.mimetype().to_string();
    Some(
        (
            StatusCode::OK,
            [(header::CONTENT_TYPE, mime)],
            Body::from(file.data.into_owned()),
        )
            .into_response(),
    )
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "not found").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_is_always_embedded() {
        assert!(Assets::get("index.html").is_some());
    }
}
