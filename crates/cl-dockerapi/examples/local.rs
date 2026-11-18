use std::sync::Arc;
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).ok_or("usage: local <socket path>")?;
    let docker = cl_docker::Docker::connect()?;
    cl_dockerapi::serve(
        std::path::Path::new(&path),
        Arc::new(cl_dockerapi::LocalBackend::new(docker)),
        std::future::pending(),
    )
    .await?;
    Ok(())
}
