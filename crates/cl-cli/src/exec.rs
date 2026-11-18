//! `clouduler exec`: run a command inside a running container on whichever node
//! runs it. Same node resolution and auth as `clouduler logs`, but over a websocket
//! (`cl-api::routes::instances::internal_exec`) with a tty, like `docker exec -it`.

use std::io::IsTerminal;

use anyhow::{anyhow, bail, Context, Result};
use cl_config::ClusterConfig;
use cl_secret::ClusterKey;
use futures::{SinkExt, StreamExt};
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::client::ClientRequestBuilder;
use tokio_tungstenite::tungstenite::http::Uri;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::Connector;

use crate::logs::resolve_target;
use cl_raft::client::client_for;

/// With no command, try `bash` and fall back to `sh` inside the container: many
/// images (e.g. `alpine`) lack `bash`, and the server rejects an empty `cmd`.
fn default_shell_command(cmd: Vec<String>) -> Vec<String> {
    if !cmd.is_empty() {
        return cmd;
    }
    vec![
        "sh".to_string(),
        "-c".to_string(),
        "exec bash 2>/dev/null || exec sh".to_string(),
    ]
}

pub async fn run(
    cfg: &ClusterConfig,
    target: &str,
    cmd: Vec<String>,
    user: Option<String>,
    via: Option<&str>,
) -> Result<()> {
    let state = client_for(cfg, via)?
        .state()
        .await
        .map_err(|e| anyhow!("{e}"))?;
    let mut assignments = resolve_target(&state, target)?;
    if assignments.len() > 1 {
        let ids = assignments
            .iter()
            .map(|a| a.instance.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "{target} has {} running instances; name one directly: {ids}",
            assignments.len()
        );
    }
    let assignment = assignments.remove(0);
    let cmd = default_shell_command(cmd);

    let key = ClusterKey::load(&cfg.secrets.key_file).with_context(|| {
        format!(
            "loading the cluster key from {} to authenticate to the node running this",
            cfg.secrets.key_file.display()
        )
    })?;

    let base = cl_update::peers::base_url(cfg, &assignment.node).ok_or_else(|| {
        anyhow!(
            "node {} has no address on the internal network",
            assignment.node
        )
    })?;
    let ws_base = base.replacen("https://", "wss://", 1);

    let tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let qs = serde_urlencoded::to_string(ExecQuery {
        cmd: serde_json::to_string(&cmd)?,
        tty,
        user,
    })?;
    let path = format!("/api/instances/internal/{}/exec?{qs}", assignment.instance);

    let uri: Uri = format!("{ws_base}{path}")
        .parse()
        .with_context(|| format!("building the exec url for {}", assignment.node))?;
    let request = cl_update::internal_client::auth_headers(&key, "GET", &path, b"")
        .into_iter()
        .fold(ClientRequestBuilder::new(uri), |request, (name, value)| {
            request.with_header(name, value)
        });

    let tls_config = insecure_tls_config();
    let (ws_stream, _) = tokio_tungstenite::connect_async_tls_with_config(
        request,
        None,
        false,
        Some(Connector::Rustls(tls_config)),
    )
    .await
    .with_context(|| {
        format!(
            "connecting to {} for {}'s exec session",
            assignment.node, assignment.instance
        )
    })?;

    let raw_terminal = tty.then(RawTerminal::enable).transpose()?;
    let result = bridge(ws_stream).await;
    drop(raw_terminal);

    // The stdin task in `bridge` is a blocking read on a threadpool thread that
    // `.abort()` cannot interrupt; returning would make the tokio runtime wait for
    // it until another keypress. Exit directly instead.
    match result {
        Ok(code) => std::process::exit(code.unwrap_or(0)),
        Err(e) => {
            eprintln!("Error: {e:?}");
            std::process::exit(1);
        }
    }
}

#[derive(Serialize)]
struct ExecQuery {
    cmd: String,
    tty: bool,
    user: Option<String>,
}

/// Returns the command's exit code when the server reported one.
async fn bridge<S>(ws_stream: tokio_tungstenite::WebSocketStream<S>) -> Result<Option<i32>>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut write, mut read) = ws_stream.split();

    let stdin_task = tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let mut buf = [0u8; 4096];
        loop {
            match stdin.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if write
                        .send(Message::Binary(buf[..n].to_vec().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
        // Not `Message::Close`: that starts the close handshake, after which the
        // server can no longer send, losing output of commands run without a
        // terminal (stdin at EOF at once). An empty frame means "stdin is done".
        let _ = write.send(Message::Binary(Vec::new().into())).await;
    });

    let mut stdout = tokio::io::stdout();
    let mut server_error = None;
    let mut exit_code = None;
    while let Some(msg) = read.next().await {
        match msg {
            Ok(Message::Binary(data)) => {
                stdout.write_all(&data).await.context("writing to stdout")?;
                stdout.flush().await.context("flushing stdout")?;
            }
            Ok(Message::Text(text)) => {
                match text
                    .strip_prefix("exit:")
                    .and_then(|c| c.parse::<i32>().ok())
                {
                    Some(code) => exit_code = Some(code),
                    None => {
                        server_error = Some(text.to_string());
                        break;
                    }
                }
            }
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => {}
        }
    }
    stdin_task.abort();

    match server_error {
        Some(msg) => bail!("{msg}"),
        None => Ok(exit_code),
    }
}

/// Accepts any certificate, like `clouduler logs`: node certs are self signed until
/// ACME issues one, and the caller is authenticated by the HMAC header.
pub fn insecure_tls_config() -> std::sync::Arc<rustls::ClientConfig> {
    let config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(std::sync::Arc::new(AcceptAnyCert))
        .with_no_client_auth();
    std::sync::Arc::new(config)
}

#[derive(Debug)]
struct AcceptAnyCert;

impl rustls::client::danger::ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Puts the terminal into raw mode for the exec session; restores it on drop.
struct RawTerminal {
    original: nix::sys::termios::Termios,
}

impl RawTerminal {
    fn enable() -> Result<Self> {
        use nix::sys::termios::{cfmakeraw, tcgetattr, tcsetattr, SetArg};
        let stdin = std::io::stdin();
        let original = tcgetattr(&stdin).context("reading terminal attributes")?;
        let mut raw = original.clone();
        cfmakeraw(&mut raw);
        tcsetattr(&stdin, SetArg::TCSANOW, &raw).context("enabling raw terminal mode")?;
        Ok(Self { original })
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        let stdin = std::io::stdin();
        let _ = nix::sys::termios::tcsetattr(
            &stdin,
            nix::sys::termios::SetArg::TCSANOW,
            &self.original,
        );
    }
}
