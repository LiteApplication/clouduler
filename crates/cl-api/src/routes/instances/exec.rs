use super::*;

#[derive(Deserialize)]
pub(super) struct ExecQuery {
    /// JSON-encoded (`["sh","-c","ls"]`) so arguments can hold spaces or `&` safely in a query string.
    cmd: String,
    #[serde(default)]
    tty: bool,
    user: Option<String>,
}

impl TryFrom<ExecQuery> for ExecSpec {
    type Error = ApiError;

    fn try_from(query: ExecQuery) -> Result<Self, Self::Error> {
        let cmd: Vec<String> = serde_json::from_str(&query.cmd)
            .map_err(|e| ApiError::BadRequest(format!("cmd: {e}")))?;
        if cmd.is_empty() {
            return Err(ApiError::BadRequest("cmd must not be empty".to_string()));
        }
        Ok(ExecSpec {
            cmd,
            tty: query.tty,
            user: query.user,
        })
    }
}

/// Node-to-node only: runs a command over a websocket, raw binary frames each way
/// (stdout/stderr are combined by docker in `tty` mode, the only mode the cli uses).
pub(super) async fn internal_exec(
    _auth: InternalAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<ExecQuery>,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let instance = parse_id(&id)?;
    let spec: ExecSpec = query.try_into()?;
    let docker = state.docker().await?;
    Ok(ws.on_upgrade(move |socket| bridge_exec(socket, docker, instance.container_name(), spec)))
}

pub(super) async fn bridge_exec(
    mut socket: WebSocket,
    docker: Docker,
    container: String,
    spec: ExecSpec,
) {
    let mut session = match docker.exec(&container, spec).await {
        Ok(session) => session,
        Err(e) => {
            let _ = socket
                .send(Message::Text(format!("exec: {e}").into()))
                .await;
            return;
        }
    };

    // A non-interactive caller (piped stdin) hits stdin EOF almost immediately, often before any
    // output. That only means "no more input": close `session.input` and keep relaying output
    // rather than tearing down the session.
    let exec_id = session.id.clone();
    let mut input = Some(session.input);
    let mut finished = false;
    loop {
        tokio::select! {
            incoming = socket.recv(), if input.is_some() => {
                match incoming {
                    // An empty frame is the client's stdin EOF (`cl-cli` `exec::bridge`): half-close stdin,
                    // keep relaying output.
                    Some(Ok(Message::Binary(data))) if data.is_empty() => {
                        let _ = input.as_mut().unwrap().shutdown().await;
                        input = None;
                    }
                    Some(Ok(Message::Binary(data))) => {
                        if input.as_mut().unwrap().write_all(&data).await.is_err() {
                            input = None;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => input = None,
                }
            }
            chunk = session.output.next() => {
                match chunk {
                    Some(Ok(chunk)) => {
                        if socket.send(Message::Binary(chunk.bytes)).await.is_err() {
                            break;
                        }
                    }
                    Some(Err(e)) => {
                        let _ = socket.send(Message::Text(format!("exec: {e}").into())).await;
                        break;
                    }
                    None => {
                        finished = true;
                        break;
                    }
                }
            }
        }
    }
    // Lets the client exit with the same code (`exit:<n>`; other text frames are errors).
    if finished {
        if let Ok(Some(code)) = docker.exec_exit_code(&exec_id).await {
            let _ = socket
                .send(Message::Text(format!("exit:{code}").into()))
                .await;
        }
    }
    let _ = socket.send(Message::Close(None)).await;
}

#[derive(Deserialize)]
pub(super) struct CaptureQuery {
    /// Defaults to the overlay network `clouduler stack deploy` builds (`cl_sched::run::overlay_network_name`),
    /// the one the proxy forwards to.
    network: Option<String>,
    /// Raw BPF filter (`tcpdump` syntax), space-split; a filter needing quoting is unsupported.
    filter: Option<String>,
    /// Stream raw pcap bytes (`tcpdump -w -`) instead of text lines.
    #[serde(default)]
    pcap: bool,
}

/// Node-to-node only: runs `tcpdump` on this node's bridge for one of the stack's overlay
/// networks (`cl_net::vxlan::bridge_name`) and streams output over a websocket like `internal_exec`.
pub(super) async fn internal_capture(
    _auth: InternalAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<CaptureQuery>,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let instance = parse_id(&id)?;
    let cluster_state = state.cluster.snapshot().await;
    let stack = cluster_state
        .stacks
        .get(&instance.stack)
        .ok_or_else(|| ApiError::NotFound(format!("stack {} not found", instance.stack)))?;
    let file = StackFile::parse(&stack.spec).map_err(|e| ApiError::Internal(e.to_string()))?;

    let network = match query.network {
        Some(n) => n,
        None => {
            let cfg = state.cluster_config.read().await;
            cl_sched::run::overlay_network_name(&file, &cfg)
                .ok_or_else(|| {
                    ApiError::BadRequest(format!(
                        "stack {} has no subnetted or public network to pick a default from; pass --network",
                        instance.stack
                    ))
                })?
                .to_string()
        }
    };
    if !file.networks.contains_key(&network) {
        return Err(ApiError::BadRequest(format!(
            "stack {} declares no network {network}",
            instance.stack
        )));
    }

    let overlay = cl_sched::run::overlay_name(&instance.stack, &network);
    let bridge = cl_net::vxlan::bridge_name(&overlay);
    Ok(ws.on_upgrade(move |socket| bridge_capture(socket, bridge, query.pcap, query.filter)))
}

pub(super) async fn bridge_capture(
    mut socket: WebSocket,
    iface: String,
    pcap: bool,
    filter: Option<String>,
) {
    let mut cmd = Command::new("tcpdump");
    cmd.arg("-i").arg(&iface).arg("-nn").kill_on_drop(true);
    if pcap {
        cmd.arg("-w").arg("-");
    } else {
        cmd.arg("-l").arg("-U");
    }
    if let Some(filter) = &filter {
        cmd.args(filter.split_whitespace());
    }
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            let _ = socket
                .send(Message::Text(format!("tcpdump: {e}").into()))
                .await;
            let _ = socket.send(Message::Close(None)).await;
            return;
        }
    };
    let mut stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");

    // Relayed on its own task: once stderr hits EOF a `select!` arm would spin on `Ok(0)` forever.
    let (err_tx, mut err_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut stderr = stderr;
        let mut buf = [0u8; 4096];
        loop {
            match stderr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if err_tx
                        .send(String::from_utf8_lossy(&buf[..n]).into_owned())
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });

    let mut buf = [0u8; 8192];
    let mut err_open = true;
    loop {
        tokio::select! {
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {}
                }
            }
            n = stdout.read(&mut buf) => {
                match n {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let msg = if pcap { Message::Binary(buf[..n].to_vec().into()) } else { Message::Text(String::from_utf8_lossy(&buf[..n]).into_owned().into()) };
                        if socket.send(msg).await.is_err() {
                            break;
                        }
                    }
                }
            }
            // Disabled once the sender task ends, instead of spinning on a ready `None`.
            text = err_rx.recv(), if err_open => {
                match text {
                    Some(text) => {
                        if socket.send(Message::Text(format!("tcpdump: {text}").into())).await.is_err() {
                            break;
                        }
                    }
                    None => err_open = false,
                }
            }
        }
    }
    let _ = child.start_kill();
    let _ = socket.send(Message::Close(None)).await;
}

// Buffered inspection for the MCP layer. Public halves run locally or forward to the owning
// node. Nothing is redacted here: that is the MCP layer's job on the serving node.

pub(super) fn default_exec_timeout() -> u64 {
    EXEC_RUN_DEFAULT_SECS
}

pub(super) fn default_exec_output() -> usize {
    EXEC_RUN_DEFAULT_OUTPUT
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ExecRunRequest {
    pub(crate) cmd: Vec<String>,
    #[serde(default)]
    pub(crate) user: Option<String>,
    /// Written to stdin, which is then closed; without it stdin closes immediately.
    #[serde(default)]
    pub(crate) stdin: Option<String>,
    #[serde(default = "default_exec_timeout")]
    pub(crate) timeout_secs: u64,
    #[serde(default = "default_exec_output")]
    pub(crate) max_output: usize,
    /// Run in a one-off container replacing the instance's own for the duration; `stdin` unsupported.
    #[serde(default)]
    pub(crate) standalone: bool,
    /// Image for the standalone container instead of the instance's own.
    #[serde(default)]
    pub(crate) image: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ExecRunResult {
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) exit_code: Option<i64>,
    pub(crate) timed_out: bool,
    pub(crate) stdout_truncated: bool,
    pub(crate) stderr_truncated: bool,
    #[serde(default)]
    pub(crate) duration_ms: u64,
}

/// Output past the cap is still read and discarded so the command is not blocked on a full pipe.
pub(super) struct CappedBuf {
    data: Vec<u8>,
    cap: usize,
    truncated: bool,
}

impl CappedBuf {
    fn new(cap: usize) -> Self {
        Self {
            data: Vec::new(),
            cap,
            truncated: false,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        let room = self.cap.saturating_sub(self.data.len());
        if bytes.len() > room {
            self.truncated = true;
        }
        self.data.extend_from_slice(&bytes[..bytes.len().min(room)]);
    }

    fn into_text(self) -> (String, bool) {
        (
            String::from_utf8_lossy(&self.data).into_owned(),
            self.truncated,
        )
    }
}

/// Not a tty, so stdout and stderr stay separate. On timeout it stops reading but the
/// command keeps running: docker cannot kill an exec, so side effects may still land.
pub(crate) async fn exec_run_local(
    state: &AppState,
    instance: &InstanceId,
    req: ExecRunRequest,
) -> Result<ExecRunResult, ApiError> {
    if req.cmd.is_empty() {
        return Err(ApiError::BadRequest("cmd must not be empty".to_string()));
    }
    if req.stdin.as_deref().map_or(0, str::len) > MAX_BUFFERED_BYTES {
        return Err(ApiError::BadRequest(format!(
            "stdin is limited to {MAX_BUFFERED_BYTES} bytes"
        )));
    }

    let docker = state.docker().await?;
    if req.standalone {
        if req.stdin.is_some() {
            return Err(ApiError::BadRequest(
                "a standalone action takes no stdin".to_string(),
            ));
        }
        return exec_standalone(state, &docker, instance, req).await;
    }
    if req.image.is_some() {
        return Err(ApiError::BadRequest(
            "an image override needs a standalone action".to_string(),
        ));
    }
    exec_run_docker(&docker, instance, req).await
}

pub(super) const STANDALONE_STOP_GRACE: Duration = Duration::from_secs(30);

/// Runs `req.cmd` in a one-off copy with the instance stopped meanwhile. The copy exists the
/// whole time, which keeps the reconciler from restarting the instance (`Docker::action_holds`);
/// the instance is restarted at the end if it was running.
pub(super) async fn exec_standalone(
    state: &AppState,
    docker: &Docker,
    instance: &InstanceId,
    req: ExecRunRequest,
) -> Result<ExecRunResult, ApiError> {
    let main = instance.container_name();
    let name = format!("{main}-action");
    let main_state = docker
        .inspect(&main)
        .await
        .map_err(docker_error)?
        .ok_or_else(|| ApiError::NotFound(format!("container {main} does not exist")))?;
    if let Some(old) = docker.inspect(&name).await.map_err(docker_error)? {
        if matches!(
            old.status,
            cl_docker::RunState::Running | cl_docker::RunState::Created
        ) {
            return Err(ApiError::Conflict(format!(
                "an action is already running on {instance}"
            )));
        }
        docker.remove(&name, true).await.map_err(docker_error)?;
    }

    let started = std::time::Instant::now();
    let timeout = Duration::from_secs(req.timeout_secs.clamp(1, EXEC_RUN_MAX_SECS));
    let cap = req.max_output.clamp(1, MAX_BUFFERED_BYTES);
    if let Some(image) = &req.image {
        if !docker.image_exists(image).await.map_err(docker_error)? {
            let registries = crate::routes::registries::load(state).await;
            docker
                .pull_as(image, registries.for_image(image))
                .await
                .map_err(docker_error)?;
        }
    }
    // Created before the instance stops so the reconciler never sees a gap.
    docker
        .create_action_container(&main, &name, req.cmd, req.user, req.image)
        .await
        .map_err(docker_error)?;

    let result = run_standalone(docker, &main, &name, main_state.running, timeout, cap).await;

    if let Err(e) = docker.remove(&name, true).await {
        warn!(%instance, error = %e, "could not remove the action container");
    }
    if main_state.running {
        if let Err(e) = docker.start(&main).await {
            let up = matches!(docker.inspect(&main).await, Ok(Some(s)) if s.running);
            if !up {
                warn!(%instance, error = %e, "could not start the instance after its action; the reconciler will retry");
            }
        }
    }
    let mut result = result?;
    result.duration_ms = started.elapsed().as_millis() as u64;
    Ok(result)
}

pub(super) async fn run_standalone(
    docker: &Docker,
    main: &str,
    name: &str,
    main_running: bool,
    timeout: Duration,
    cap: usize,
) -> Result<ExecRunResult, ApiError> {
    if main_running {
        docker
            .stop(main, STANDALONE_STOP_GRACE)
            .await
            .map_err(docker_error)?;
    }
    docker.start(name).await.map_err(docker_error)?;
    let (exit_code, timed_out) = match tokio::time::timeout(timeout, docker.wait(name)).await {
        Ok(Ok(code)) => (Some(code), false),
        Ok(Err(e)) => return Err(docker_error(e)),
        Err(_) => (None, true),
    };

    let mut out = CappedBuf::new(cap);
    let mut err = CappedBuf::new(cap);
    let mut logs = Box::pin(docker.logs(name, &LogOptions::default()));
    while let Some(line) = logs.next().await {
        let line = line.map_err(docker_error)?;
        let (buf, text) = match line.stream {
            cl_docker::LogStream::Stdout => (&mut out, line.text),
            cl_docker::LogStream::Stderr => (&mut err, line.text),
        };
        buf.push(text.as_bytes());
        buf.push(b"\n");
    }
    if let Some(line) = docker.start_failure_line(name).await {
        err.push(line.text.as_bytes());
        err.push(b"\n");
    }
    let (stdout, stdout_truncated) = out.into_text();
    let (stderr, stderr_truncated) = err.into_text();
    Ok(ExecRunResult {
        stdout,
        stderr,
        exit_code,
        timed_out,
        stdout_truncated,
        stderr_truncated,
        duration_ms: 0,
    })
}

pub(super) async fn exec_run_docker(
    docker: &Docker,
    instance: &InstanceId,
    req: ExecRunRequest,
) -> Result<ExecRunResult, ApiError> {
    if req.cmd.is_empty() {
        return Err(ApiError::BadRequest("cmd must not be empty".to_string()));
    }
    let started = std::time::Instant::now();
    let stdin = req.stdin.unwrap_or_default().into_bytes();
    let timeout = Duration::from_secs(req.timeout_secs.clamp(1, EXEC_RUN_MAX_SECS));
    let cap = req.max_output.clamp(1, MAX_BUFFERED_BYTES);
    let session = docker
        .exec(
            &instance.container_name(),
            ExecSpec {
                cmd: req.cmd,
                tty: false,
                user: req.user,
            },
        )
        .await
        .map_err(docker_error)?;
    let exec_id = session.id;
    let mut output = session.output;

    // Own task: a command that fills stdout before reading all of stdin would deadlock this loop.
    let mut input = session.input;
    tokio::spawn(async move {
        if !stdin.is_empty() {
            let _ = input.write_all(&stdin).await;
        }
        let _ = input.shutdown().await;
    });

    let mut out = CappedBuf::new(cap);
    let mut err = CappedBuf::new(cap);
    let deadline = tokio::time::Instant::now() + timeout;
    let mut timed_out = false;
    loop {
        match tokio::time::timeout_at(deadline, output.next()).await {
            Err(_) => {
                timed_out = true;
                break;
            }
            Ok(None) => break,
            Ok(Some(Err(e))) => return Err(docker_error(e)),
            Ok(Some(Ok(chunk))) => match chunk.stream {
                cl_docker::ExecStream::Stdout => out.push(&chunk.bytes),
                cl_docker::ExecStream::Stderr => err.push(&chunk.bytes),
            },
        }
    }

    let exit_code = if timed_out {
        None
    } else {
        docker.exec_exit_code(&exec_id).await.ok().flatten()
    };
    let (stdout, stdout_truncated) = out.into_text();
    let (stderr, stderr_truncated) = err.into_text();
    Ok(ExecRunResult {
        stdout,
        stderr,
        exit_code,
        timed_out,
        stdout_truncated,
        stderr_truncated,
        duration_ms: started.elapsed().as_millis() as u64,
    })
}

pub(super) fn internal_exec_run_path(instance: &InstanceId) -> String {
    format!("/api/instances/internal/{instance}/exec-run")
}

/// See `exec_run_local` for the timeout caveat.
pub(crate) async fn exec_run(
    state: &AppState,
    instance: &InstanceId,
    req: ExecRunRequest,
) -> Result<ExecRunResult, ApiError> {
    forward_or_local(
        state,
        instance,
        exec_run_local(state, instance, req.clone()),
        |node| async move {
            internal_peer::post_json_body(state, &node, &internal_exec_run_path(instance), &req)
                .await
        },
    )
    .await
}

/// Takes a body, so the signed body hash is checked against what was read.
pub(super) async fn internal_exec_run(
    auth: InternalBodyAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Json<ExecRunResult>, ApiError> {
    auth.check(&body)?;
    let req: ExecRunRequest = serde_json::from_slice(&body)
        .map_err(|e| ApiError::BadRequest(format!("invalid exec-run request: {e}")))?;
    Ok(Json(exec_run_local(&state, &parse_id(&id)?, req).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capped_buf_keeps_the_prefix_and_flags_the_rest() {
        let mut buf = CappedBuf::new(5);
        buf.push(b"abc");
        assert!(!buf.truncated);
        buf.push(b"defgh");
        assert!(buf.truncated);
        let (text, truncated) = buf.into_text();
        assert_eq!(text, "abcde");
        assert!(truncated);
    }

    #[test]
    fn capped_buf_exactly_at_the_cap_is_not_truncated() {
        let mut buf = CappedBuf::new(3);
        buf.push(b"abc");
        assert!(!buf.truncated);
        buf.push(b"");
        assert!(!buf.truncated);
    }
}
