//! Running a command inside an already running container (`docker exec`).

use std::pin::Pin;

use bollard::container::LogOutput;
use bollard::exec::{CreateExecOptions, StartExecOptions, StartExecResults};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use tokio::io::AsyncWrite;

use crate::client::Docker;
use crate::error::{classify, DockerError, Result};

/// What to run and how.
#[derive(Debug, Clone)]
pub struct ExecSpec {
    pub cmd: Vec<String>,
    /// Allocates a pseudo-tty and asks docker to combine stdout/stderr into
    /// one stream, the way an interactive shell expects. Without it the two
    /// stay separate frames in `ExecSession::output`.
    pub tty: bool,
    /// `user`, `user:group`, `uid` or `uid:gid`. Defaults to the image's own
    /// user when unset.
    pub user: Option<String>,
}

/// One byte, tagged with which of the container's streams it came from.
/// Meaningless once `ExecSpec::tty` is set, since docker itself no longer
/// distinguishes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecStream {
    Stdout,
    Stderr,
}

pub struct ExecChunk {
    pub stream: ExecStream,
    pub bytes: Bytes,
}

/// A running exec: its output as it arrives, and a sink for stdin. Dropping
/// `input` closes the container's stdin, same as a real terminal hanging up.
pub struct ExecSession {
    pub output: Pin<Box<dyn Stream<Item = Result<ExecChunk>> + Send>>,
    pub input: Pin<Box<dyn AsyncWrite + Send>>,
}

impl Docker {
    pub async fn exec(&self, container: &str, spec: ExecSpec) -> Result<ExecSession> {
        let created = self
            .inner
            .create_exec(
                container,
                CreateExecOptions {
                    attach_stdin: Some(true),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    tty: Some(spec.tty),
                    cmd: Some(spec.cmd),
                    user: spec.user,
                    ..Default::default()
                },
            )
            .await
            .map_err(classify)?;

        let started = self
            .inner
            .start_exec(
                &created.id,
                Some(StartExecOptions {
                    detach: false,
                    tty: spec.tty,
                    ..Default::default()
                }),
            )
            .await
            .map_err(classify)?;

        match started {
            StartExecResults::Attached { output, input } => Ok(ExecSession {
                output: Box::pin(output.map(|item| item.map(from_log_output).map_err(classify))),
                input,
            }),
            // Cannot happen: `detach` above is always false.
            StartExecResults::Detached => Err(DockerError::Other("docker detached an attached exec".to_string())),
        }
    }
}

fn from_log_output(output: LogOutput) -> ExecChunk {
    match output {
        LogOutput::StdOut { message } => ExecChunk { stream: ExecStream::Stdout, bytes: message },
        LogOutput::StdErr { message } => ExecChunk { stream: ExecStream::Stderr, bytes: message },
        // A tty exec's combined stream, and stdin echoed back, both land here.
        LogOutput::StdIn { message } | LogOutput::Console { message } => {
            ExecChunk { stream: ExecStream::Stdout, bytes: message }
        }
    }
}
