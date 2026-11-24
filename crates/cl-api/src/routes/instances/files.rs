use super::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct FileRead {
    pub(crate) path: String,
    pub(crate) size: u64,
    pub(crate) truncated: bool,
    pub(crate) is_binary: bool,
    pub(crate) content: Option<String>,
    pub(crate) kind: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) entries: Vec<String>,
}

/// Binary means a NUL or invalid utf-8 in the first 8 KiB; a multi-byte char merely cut by that
/// window (or the caller's size cap) is not evidence of binary.
pub(super) fn looks_binary(data: &[u8]) -> bool {
    let head = &data[..data.len().min(BINARY_SNIFF_BYTES)];
    if head.contains(&0) {
        return true;
    }
    match std::str::from_utf8(head) {
        Ok(_) => false,
        // `error_len() == None` is only innocent when the sniff window really cut the data short.
        Err(e) => !(e.error_len().is_none() && data.len() > head.len()),
    }
}

pub(super) fn file_read_from(path: &str, read: cl_docker::PathRead) -> FileRead {
    let kind = match read.kind {
        cl_docker::PathKind::File => "file",
        cl_docker::PathKind::Dir => "dir",
        cl_docker::PathKind::Symlink => "symlink",
        cl_docker::PathKind::Other => "other",
    };
    let (is_binary, content) = if read.kind == cl_docker::PathKind::File {
        if looks_binary(&read.data) {
            (true, None)
        } else {
            (
                false,
                Some(String::from_utf8_lossy(&read.data).into_owned()),
            )
        }
    } else {
        (false, None)
    };
    FileRead {
        path: path.to_string(),
        size: read.size,
        truncated: read.truncated,
        is_binary,
        content,
        kind: kind.to_string(),
        entries: read.entries,
    }
}

/// Uses docker's archive endpoint rather than `exec cat`, so it works in shell-less images.
pub(crate) async fn read_file_local(
    state: &AppState,
    instance: &InstanceId,
    path: &str,
    max: usize,
) -> Result<FileRead, ApiError> {
    if !path.starts_with('/') || path.contains('\0') {
        return Err(ApiError::BadRequest(
            "path must be absolute and must not contain NUL".to_string(),
        ));
    }
    let max = max.clamp(1, MAX_BUFFERED_BYTES);
    let docker = state.docker().await?;
    let read = docker
        .read_path(&instance.container_name(), path, max, FILE_LIST_LIMIT)
        .await
        .map_err(docker_error)?;
    Ok(file_read_from(path, read))
}

#[derive(Deserialize, Serialize)]
pub(super) struct FileQuery {
    path: String,
    max: Option<usize>,
}

pub(super) fn internal_file_path(instance: &InstanceId, query: &FileQuery) -> String {
    let qs = serde_urlencoded::to_string(query).unwrap_or_default();
    format!("/api/instances/internal/{instance}/file?{qs}")
}

/// `max` defaults to 256 KiB, capped at 1 MiB.
pub(crate) async fn read_file(
    state: &AppState,
    instance: &InstanceId,
    path: &str,
    max: Option<usize>,
) -> Result<FileRead, ApiError> {
    let node = owning_node(state, instance).await?;
    let max = max.unwrap_or(FILE_READ_DEFAULT).min(MAX_BUFFERED_BYTES);
    if node == state.node_name {
        return read_file_local(state, instance, path, max).await;
    }
    let query = FileQuery {
        path: path.to_string(),
        max: Some(max),
    };
    internal_peer::fetch_json(state, &node, &internal_file_path(instance, &query)).await
}

pub(super) async fn internal_file(
    _auth: InternalAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<FileQuery>,
) -> Result<Json<FileRead>, ApiError> {
    let max = query.max.unwrap_or(FILE_READ_DEFAULT);
    Ok(Json(
        read_file_local(&state, &parse_id(&id)?, &query.path, max).await?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nul_means_binary() {
        assert!(looks_binary(b"abc\0def"));
    }

    #[test]
    fn plain_and_multibyte_text_is_not_binary() {
        assert!(!looks_binary("héllo wörld ✓".as_bytes()));
        assert!(!looks_binary(b""));
    }

    #[test]
    fn invalid_utf8_means_binary() {
        assert!(looks_binary(&[0x66, 0xff, 0xfe, 0x6f]));
    }

    #[test]
    fn a_character_cut_by_the_sniff_window_is_not_binary() {
        // 'é' is two bytes; put its first byte on the last sniffed byte.
        let mut data = vec![b'a'; BINARY_SNIFF_BYTES - 1];
        data.extend_from_slice("é".as_bytes());
        data.extend_from_slice(b"tail");
        assert!(!looks_binary(&data));
    }

    #[test]
    fn a_character_cut_at_the_very_end_of_the_data_is_binary() {
        // Nothing follows, so the window did not cut it; the data is just
        // malformed.
        assert!(looks_binary(&[b'a', 0xc3]));
    }

    #[test]
    fn file_read_reports_a_directory_without_content() {
        let read = cl_docker::PathRead {
            kind: cl_docker::PathKind::Dir,
            size: 4096,
            data: Vec::new(),
            truncated: false,
            entries: vec!["a".into(), "b".into()],
        };
        let out = file_read_from("/etc", read);
        assert_eq!(out.kind, "dir");
        assert!(out.content.is_none() && !out.is_binary);
        assert_eq!(out.entries, vec!["a", "b"]);
    }

    #[test]
    fn file_read_withholds_binary_content() {
        let read = cl_docker::PathRead {
            kind: cl_docker::PathKind::File,
            size: 3,
            data: vec![0, 1, 2],
            truncated: false,
            entries: Vec::new(),
        };
        let out = file_read_from("/bin/x", read);
        assert!(out.is_binary && out.content.is_none());
    }
}
