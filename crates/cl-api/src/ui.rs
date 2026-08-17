//! Serves the React build from `ui/dist`, with an SPA fallback so
//! client-side routing works on a hard refresh of any `/x/y` path.
//!
//! `CL_API_UI_DIR` is set by `build.rs`: it points at the real `ui/dist` when
//! that build exists, or a placeholder page under `OUT_DIR` otherwise, so
//! this crate always compiles.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "$CL_API_UI_DIR"]
struct Assets;

/// Fallback handler: serves the requested path out of the embedded assets,
/// falling back to `index.html` for anything not found so client-side routes
/// like `/stacks/gitlab` resolve to the app shell.
pub async fn serve(request: Request) -> Response {
    let path = request.uri().path().trim_start_matches('/');
    serve_path(path).unwrap_or_else(|| serve_path("index.html").unwrap_or_else(not_found))
}

fn serve_path(path: &str) -> Option<Response> {
    let file = Assets::get(path)?;
    // rust-embed's default `mime-guess` feature fills this in from the
    // extension, so no separate mime lookup is needed here.
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
        // Either the real ui build or build.rs's placeholder must provide this.
        assert!(Assets::get("index.html").is_some());
    }
}
