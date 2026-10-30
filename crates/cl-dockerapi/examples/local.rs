use std::sync::Arc;
#[tokio::main]
async fn main() {
    let path = std::env::args().nth(1).unwrap();
    let docker = cl_docker::Docker::connect().unwrap();
    cl_dockerapi::serve(
        std::path::Path::new(&path),
        Arc::new(cl_dockerapi::LocalBackend::new(docker)),
        std::future::pending(),
    )
    .await
    .unwrap();
}
