use super::*;

#[derive(Deserialize, Serialize)]
pub(super) struct LogsQuery {
    #[serde(default)]
    follow: bool,
    tail: Option<u32>,
    since: Option<i64>,
}

pub(super) async fn logs(
    _identity: Identity,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<LogsQuery>,
) -> Result<Response, ApiError> {
    let instance = parse_id(&id)?;
    let cluster_state = state.cluster.snapshot().await;
    let assignment = cluster_state
        .assignments
        .get(&instance)
        .ok_or_else(|| ApiError::NotFound(format!("no instance {id}")))?;
    if assignment.node != state.node_name {
        let node = assignment.node.clone();
        let qs = serde_urlencoded::to_string(&query).unwrap_or_default();
        let path = format!("{}?{qs}", internal_logs_path(&instance));
        return internal_peer::proxy_stream(&state, &node, &path).await;
    }
    serve_logs(&state, &instance, query).await
}

pub(super) fn internal_logs_path(instance: &InstanceId) -> String {
    format!("/api/instances/internal/{instance}/logs")
}

pub(super) async fn serve_logs(
    state: &AppState,
    instance: &InstanceId,
    query: LogsQuery,
) -> Result<Response, ApiError> {
    let opts = LogOptions {
        follow: query.follow,
        tail: query.tail.map(Tail::Lines).unwrap_or(Tail::Lines(200)),
        since: query.since,
    };
    let container_name = instance.container_name();
    let docker = state.docker().await?;
    let log_stream = async_stream::stream! {
        let stream = docker.logs(&container_name, &opts);
        futures::pin_mut!(stream);
        while let Some(item) = futures::StreamExt::next(&mut stream).await {
            yield item;
        }
        if let Some(line) = docker.start_failure_line(&container_name).await {
            yield Ok(line);
        }
    };
    Ok(stream::container_log_events(log_stream).into_response())
}

pub(super) async fn internal_logs(
    _auth: InternalAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<LogsQuery>,
) -> Result<Response, ApiError> {
    let instance = parse_id(&id)?;
    serve_logs(&state, &instance, query).await
}

pub(super) fn internal_stats_path(instance: &InstanceId) -> String {
    format!("/api/instances/internal/{instance}/stats")
}

pub(super) async fn stats(
    _identity: Identity,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let instance = parse_id(&id)?;
    let cluster_state = state.cluster.snapshot().await;
    let assignment = cluster_state
        .assignments
        .get(&instance)
        .ok_or_else(|| ApiError::NotFound(format!("no instance {id}")))?;
    if assignment.node != state.node_name {
        let node = assignment.node.clone();
        return internal_peer::proxy_stream(&state, &node, &internal_stats_path(&instance)).await;
    }
    serve_stats(&instance).await
}

pub(super) async fn serve_stats(instance: &InstanceId) -> Result<Response, ApiError> {
    let samples = crate::docker_stats::stream(instance.container_name()).await?;
    Ok(stream::container_stat_events(samples).into_response())
}

pub(super) async fn internal_stats(
    _auth: InternalAuth,
    State(_state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let instance = parse_id(&id)?;
    serve_stats(&instance).await
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LogLine {
    pub(crate) stream: String,
    pub(crate) line: String,
    pub(crate) timestamp: Option<String>,
}

/// Parses a `follow=false` SSE log response (`stream::container_log_events`). Keep-alives and unknown
/// events are skipped; an `error` event fails the parse, since a silently shortened tail would mislead.
pub(super) fn parse_sse_logs(body: &str) -> Result<Vec<LogLine>, String> {
    let body = body.replace("\r\n", "\n");
    let mut lines = Vec::new();
    for block in body.split("\n\n") {
        let mut event = "message";
        let mut data: Vec<&str> = Vec::new();
        for field in block.lines() {
            if let Some(v) = field.strip_prefix("event:") {
                event = v.trim();
            } else if let Some(v) = field.strip_prefix("data:") {
                data.push(v.strip_prefix(' ').unwrap_or(v));
            }
        }
        if data.is_empty() {
            continue;
        }
        let data = data.join("\n");
        match event {
            "stdout" | "stderr" => {
                #[derive(Deserialize)]
                struct Payload {
                    timestamp: Option<String>,
                    text: String,
                }
                let (timestamp, text) = match serde_json::from_str::<Payload>(&data) {
                    Ok(p) => (p.timestamp, p.text),
                    Err(_) => (None, data),
                };
                lines.push(LogLine {
                    stream: event.to_string(),
                    line: text.trim_end_matches(['\n', '\r']).to_string(),
                    timestamp,
                });
            }
            "error" => return Err(data),
            _ => {}
        }
    }
    Ok(lines)
}

/// Local instances read docker; remote ones use the owner's `internal_logs` with `follow=false`,
/// parsing the SSE body back into lines.
pub(crate) async fn fetch_logs(
    state: &AppState,
    instance: &InstanceId,
    tail: u32,
    since: Option<i64>,
) -> Result<Vec<LogLine>, ApiError> {
    let tail = tail.min(LOG_TAIL_LIMIT);
    forward_or_local(
        state,
        instance,
        fetch_logs_local(state, instance, tail, since),
        |node| async move {
            let qs = serde_urlencoded::to_string(LogsQuery {
                follow: false,
                tail: Some(tail),
                since,
            })
            .unwrap_or_default();
            let path = format!("{}?{qs}", internal_logs_path(instance));
            let body = internal_peer::fetch_text(state, &node, &path).await?;
            parse_sse_logs(&body).map_err(ApiError::Internal)
        },
    )
    .await
}

async fn fetch_logs_local(
    state: &AppState,
    instance: &InstanceId,
    tail: u32,
    since: Option<i64>,
) -> Result<Vec<LogLine>, ApiError> {
    let docker = state.docker().await?;
    let opts = LogOptions {
        follow: false,
        tail: Tail::Lines(tail),
        since,
    };
    let name = instance.container_name();
    let stream = docker.logs(&name, &opts);
    futures::pin_mut!(stream);
    let mut lines = Vec::new();
    while let Some(item) = stream.next().await {
        let line = item.map_err(docker_error)?;
        lines.push(LogLine {
            stream: match line.stream {
                cl_docker::LogStream::Stdout => "stdout",
                cl_docker::LogStream::Stderr => "stderr",
            }
            .to_string(),
            line: line.text.trim_end_matches(['\n', '\r']).to_string(),
            timestamp: line.timestamp,
        });
    }
    if let Some(line) = docker.start_failure_line(&name).await {
        lines.push(LogLine {
            stream: "stderr".to_string(),
            line: line.text,
            timestamp: None,
        });
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_logs_parse_both_streams_with_timestamps() {
        let body = "event: stdout\ndata: {\"timestamp\":\"2026-01-01T00:00:00Z\",\"text\":\"hello\\n\"}\n\n\
                    event: stderr\ndata: {\"timestamp\":null,\"text\":\"oops\"}\n\n";
        let lines = parse_sse_logs(body).unwrap();
        assert_eq!(
            lines,
            vec![
                LogLine {
                    stream: "stdout".into(),
                    line: "hello".into(),
                    timestamp: Some("2026-01-01T00:00:00Z".into()),
                },
                LogLine {
                    stream: "stderr".into(),
                    line: "oops".into(),
                    timestamp: None,
                },
            ]
        );
    }

    #[test]
    fn sse_logs_skip_keepalives_and_accept_crlf() {
        let body =
            ": keep-alive\r\n\r\nevent: stdout\r\ndata: {\"timestamp\":null,\"text\":\"a\"}\r\n\r\n";
        let lines = parse_sse_logs(body).unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].line, "a");
    }

    #[test]
    fn sse_logs_error_event_fails_the_parse() {
        let body = "event: stdout\ndata: {\"timestamp\":null,\"text\":\"a\"}\n\nevent: error\ndata: docker went away\n\n";
        assert_eq!(parse_sse_logs(body).unwrap_err(), "docker went away");
    }

    #[test]
    fn sse_logs_empty_body_is_no_lines() {
        assert!(parse_sse_logs("").unwrap().is_empty());
    }
}
