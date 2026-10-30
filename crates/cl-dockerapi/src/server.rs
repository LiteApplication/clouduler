//! The HTTP side: a unix socket speaking the slice of the docker engine API
//! that `route` allows.

use std::convert::Infallible;
use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt;
use http_body_util::{combinators::UnsyncBoxBody, BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::json;
use tokio::net::UnixListener;
use tracing::{debug, warn};

use crate::backend::{BackendError, ImageBackend};
use crate::route::{route, Refusal, Route};

type Body = UnsyncBoxBody<Bytes, std::io::Error>;

/// The engine API version this socket claims. Clients negotiate down from it.
const API_VERSION: &str = "1.45";

fn full(status: StatusCode, content_type: &str, body: impl Into<Bytes>) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("Content-Type", content_type)
        .header("Api-Version", API_VERSION)
        .header("OSType", "linux")
        .header("Server", "clouduler")
        .body(
            Full::new(body.into())
                .map_err(|never| match never {})
                .boxed_unsync(),
        )
        .expect("static response parts are valid")
}

fn message(status: StatusCode, msg: &str) -> Response<Body> {
    full(
        status,
        "application/json",
        json!({ "message": msg }).to_string(),
    )
}

fn json_ok(value: &serde_json::Value) -> Response<Body> {
    full(StatusCode::OK, "application/json", value.to_string())
}

fn backend_error(e: BackendError) -> Response<Body> {
    match e {
        BackendError::NotFound(m) => message(StatusCode::NOT_FOUND, &m),
        BackendError::Other(m) => message(StatusCode::INTERNAL_SERVER_ERROR, &m),
    }
}

async fn handle(
    backend: Arc<dyn ImageBackend>,
    req: Request<Incoming>,
) -> Result<Response<Body>, Infallible> {
    let head = req.method() == Method::HEAD;
    let routed = route(req.method(), req.uri().path(), req.uri().query());
    let response = match routed {
        Err(Refusal::NotPermitted) => {
            debug!(method = %req.method(), path = req.uri().path(), "refused");
            message(StatusCode::FORBIDDEN, "not permitted by clouduler")
        }
        Err(Refusal::BadRequest(why)) => message(StatusCode::BAD_REQUEST, why),
        Ok(route) => dispatch(backend.as_ref(), route).await,
    };
    if head {
        // HEAD gets the headers of the GET and no body.
        let (parts, _) = response.into_parts();
        return Ok(Response::from_parts(
            parts,
            Full::new(Bytes::new())
                .map_err(|never| match never {})
                .boxed_unsync(),
        ));
    }
    Ok(response)
}

async fn dispatch(backend: &dyn ImageBackend, route: Route) -> Response<Body> {
    match route {
        Route::Ping => full(StatusCode::OK, "text/plain; charset=utf-8", "OK"),
        Route::Version => json_ok(&json!({
            "Version": "clouduler",
            "ApiVersion": API_VERSION,
            "MinAPIVersion": "1.24",
            "Os": "linux",
            "Arch": std::env::consts::ARCH,
        })),
        Route::Info => json_ok(&json!({
            "ID": "clouduler",
            "Name": "clouduler",
            "OSType": "linux",
            "Architecture": std::env::consts::ARCH,
            "ServerVersion": "clouduler",
        })),
        Route::ListImages => backend
            .list()
            .await
            .map_or_else(backend_error, |v| json_ok(&v)),
        Route::InspectImage(name) => backend
            .inspect(&name)
            .await
            .map_or_else(backend_error, |v| json_ok(&v)),
        Route::ImageHistory(name) => backend
            .history(&name)
            .await
            .map_or_else(backend_error, |v| json_ok(&v)),
        Route::PullImage(reference) => match backend.pull(&reference).await {
            // The engine streams progress objects; one terminal line satisfies
            // clients that only read until EOF.
            Ok(()) => full(
                StatusCode::OK,
                "application/json",
                json!({ "status": format!("Status: pulled {reference}") }).to_string() + "\n",
            ),
            Err(e) => backend_error(e),
        },
        Route::SaveImage(name) => match backend.save(&name).await {
            Ok(stream) => {
                let body = StreamBody::new(stream.map(|chunk| {
                    chunk.map(Frame::data).map_err(|e| {
                        warn!(error = %e, "image stream failed mid-transfer");
                        std::io::Error::other(e.to_string())
                    })
                }));
                Response::builder()
                    .status(StatusCode::OK)
                    .header("Content-Type", "application/x-tar")
                    .header("Api-Version", API_VERSION)
                    .body(BodyExt::boxed_unsync(body))
                    .expect("static response parts are valid")
            }
            Err(e) => backend_error(e),
        },
    }
}

/// Bind `path` and serve until `shutdown` resolves. A stale socket file from a
/// previous run is replaced; the directory is created 0750 and the socket 0660
/// so only root and the docker group can reach it, like the engine's own.
pub async fn serve(
    path: &Path,
    backend: Arc<dyn ImageBackend>,
    shutdown: impl std::future::Future<Output = ()>,
) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o750))?;
    }
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;

    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            () = &mut shutdown => break,
            accepted = listener.accept() => {
                let (stream, _) = match accepted {
                    Ok(pair) => pair,
                    Err(e) => {
                        warn!(error = %e, "docker api accept failed");
                        continue;
                    }
                };
                let backend = backend.clone();
                tokio::spawn(async move {
                    let service = hyper::service::service_fn(move |req| handle(backend.clone(), req));
                    if let Err(e) = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await
                    {
                        debug!(error = %e, "docker api connection ended");
                    }
                });
            }
        }
    }
    let _ = std::fs::remove_file(path);
    Ok(())
}
