use std::sync::Arc;

use bytes::Bytes;
use cl_dockerapi::{serve, BackendError, BackendResult, ImageBackend};
use futures::future::BoxFuture;
use futures::stream::{self, BoxStream, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

struct Fake;

impl ImageBackend for Fake {
    fn inspect<'a>(&'a self, name: &'a str) -> BoxFuture<'a, BackendResult<Value>> {
        Box::pin(async move {
            if name == "alpine:3.20" {
                Ok(json!({ "Id": "sha256:aaa", "RepoTags": ["alpine:3.20"] }))
            } else {
                Err(BackendError::NotFound(format!("No such image: {name}")))
            }
        })
    }
    fn history<'a>(&'a self, _: &'a str) -> BoxFuture<'a, BackendResult<Value>> {
        Box::pin(async { Ok(json!([])) })
    }
    fn list(&self) -> BoxFuture<'_, BackendResult<Value>> {
        Box::pin(async { Ok(json!([])) })
    }
    fn save<'a>(
        &'a self,
        name: &'a str,
    ) -> BoxFuture<'a, BackendResult<BoxStream<'static, BackendResult<Bytes>>>> {
        Box::pin(async move {
            if name != "alpine:3.20" {
                return Err(BackendError::NotFound("missing".into()));
            }
            Ok(stream::iter([
                Ok(Bytes::from_static(b"TAR")),
                Ok(Bytes::from_static(b"BYTES")),
            ])
            .boxed())
        })
    }
    fn pull<'a>(&'a self, _: &'a str) -> BoxFuture<'a, BackendResult<()>> {
        Box::pin(async { Ok(()) })
    }
}

async fn request(socket: &std::path::Path, method: &str, path: &str) -> (u16, String) {
    let mut conn = UnixStream::connect(socket).await.unwrap();
    conn.write_all(
        format!("{method} {path} HTTP/1.1\r\nHost: docker\r\nConnection: close\r\n\r\n").as_bytes(),
    )
    .await
    .unwrap();
    let mut raw = Vec::new();
    conn.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text.split(' ').nth(1).unwrap().parse().unwrap();
    let body = text.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    (status, body.to_string())
}

#[tokio::test]
async fn serves_images_and_refuses_everything_else() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("docker-api").join("docker.sock");
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = {
        let socket = socket.clone();
        tokio::spawn(async move {
            serve(&socket, Arc::new(Fake), async {
                let _ = stopped.await;
            })
            .await
        })
    };
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    assert_eq!(request(&socket, "GET", "/_ping").await, (200, "OK".into()));

    let (status, body) = request(&socket, "GET", "/v1.45/images/alpine:3.20/json").await;
    assert_eq!(status, 200);
    assert!(body.contains("sha256:aaa"), "{body}");

    assert_eq!(request(&socket, "GET", "/images/nope/json").await.0, 404);

    let (status, body) = request(&socket, "GET", "/images/alpine:3.20/get").await;
    assert_eq!(status, 200);
    // Chunked transfer framing is visible to this raw client.
    assert!(body.contains("TAR") && body.contains("BYTES"), "{body}");

    for (method, path) in [
        ("POST", "/containers/create"),
        ("GET", "/containers/json"),
        ("DELETE", "/images/alpine:3.20"),
        ("POST", "/build"),
    ] {
        let (status, body) = request(&socket, method, path).await;
        assert_eq!(status, 403, "{method} {path}");
        assert!(body.contains("not permitted by clouduler"), "{body}");
    }

    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
    assert!(!socket.exists(), "the socket file is removed on shutdown");
}
