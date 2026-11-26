//! Serves the React build that self-update staged under `<data_dir>/self-update/ui`.
//!
//! Only a directory paired with the running backend version is served (see
//! `cl_update::ui::serving_dir`); otherwise a small "not installed" page.
//! `CL_UI_DIR` overrides this with a local `ui/dist` for development.

use std::path::{Component, Path, PathBuf};

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

const WAITING_PAGE: &str = include_str!("ui_waiting.html");

/// The directory to serve right now, if any. Resolved per request so a freshly
/// staged UI shows up without restarting.
fn current_dir(data_dir: &Path) -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CL_UI_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    cl_update::ui::serving_dir(data_dir, cl_config::version())
}

pub async fn serve(data_dir: &Path, request: Request) -> Response {
    let dir = current_dir(data_dir);
    let path = request.uri().path().to_string();
    tokio::task::spawn_blocking(move || serve_from(dir.as_deref(), &path))
        .await
        .unwrap_or_else(|_| (StatusCode::INTERNAL_SERVER_ERROR, "ui read failed").into_response())
}

/// Client routes (no file extension) fall back to `index.html`; a missing file
/// that looks like an asset is a real 404 so a stale chunk URL does not get HTML.
pub(crate) fn serve_from(dir: Option<&Path>, request_path: &str) -> Response {
    let Some(dir) = dir else {
        return waiting();
    };
    let rel = request_path.trim_start_matches('/');
    let safe = Path::new(rel)
        .components()
        .all(|c| matches!(c, Component::Normal(_)));
    if !safe {
        return not_found();
    }
    if !rel.is_empty() {
        if let Some(response) = file_response(dir, rel) {
            return response;
        }
        if Path::new(rel).extension().is_some() {
            return not_found();
        }
    }
    file_response(dir, "index.html").unwrap_or_else(waiting)
}

fn file_response(dir: &Path, rel: &str) -> Option<Response> {
    let path = dir.join(rel);
    if !path.is_file() {
        return None;
    }
    let data = std::fs::read(&path).ok()?;
    let mime = mime_guess::from_path(&path).first_or_octet_stream();
    let cache = if rel == "index.html" {
        "no-cache"
    } else if rel.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    Some(
        (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, mime.to_string()),
                (header::CACHE_CONTROL, cache.to_string()),
            ],
            Body::from(data),
        )
            .into_response(),
    )
}

fn waiting() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        WAITING_PAGE,
    )
        .into_response()
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "not found").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn staged() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("assets")).unwrap();
        std::fs::write(d.path().join("index.html"), "<html>app</html>").unwrap();
        std::fs::write(d.path().join("assets/x.js"), "console.log(1)").unwrap();
        d
    }

    fn header_of(r: &Response, name: header::HeaderName) -> String {
        r.headers()[name].to_str().unwrap().to_string()
    }

    #[test]
    fn root_serves_index_no_cache() {
        let d = staged();
        let r = serve_from(Some(d.path()), "/");
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(header_of(&r, header::CACHE_CONTROL), "no-cache");
    }

    #[test]
    fn assets_are_immutable() {
        let d = staged();
        let r = serve_from(Some(d.path()), "/assets/x.js");
        assert_eq!(r.status(), StatusCode::OK);
        assert!(header_of(&r, header::CACHE_CONTROL).contains("immutable"));
    }

    #[test]
    fn missing_file_with_extension_is_404() {
        let d = staged();
        assert_eq!(
            serve_from(Some(d.path()), "/assets/gone.js").status(),
            StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn client_route_falls_back_to_index() {
        let d = staged();
        let r = serve_from(Some(d.path()), "/stacks/web");
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(header_of(&r, header::CONTENT_TYPE), "text/html");
    }

    #[test]
    fn nothing_staged_shows_waiting_page() {
        let r = serve_from(None, "/");
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(header_of(&r, header::CONTENT_TYPE).starts_with("text/html"));
    }

    #[test]
    fn traversal_is_refused() {
        let d = staged();
        assert_eq!(
            serve_from(Some(d.path()), "/../secret").status(),
            StatusCode::NOT_FOUND
        );
    }
}
