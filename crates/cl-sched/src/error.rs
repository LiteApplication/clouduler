pub type Result<T> = std::result::Result<T, SchedError>;

#[derive(Debug, thiserror::Error)]
pub enum SchedError {
    #[error("stack {stack}: {source}")]
    Stack {
        stack: String,
        #[source]
        source: cl_config::ConfigError,
    },

    /// No node satisfies the instance's requirements. The reason lists what
    /// ruled each candidate out, because "unschedulable" on its own is useless
    /// when you are trying to work out which drive you forgot to implement.
    #[error("cannot place {instance}: {reason}")]
    Unplaceable { instance: String, reason: String },

    #[error("unknown placement constraint {0}")]
    BadConstraint(String),

    #[error("{kind} {name} referenced by {instance} does not exist in the cluster")]
    MissingObject {
        kind: &'static str,
        name: String,
        instance: String,
    },

    #[error("{name} is unusable: {reason}")]
    BadObject { name: String, reason: String },

    /// This node holds no cluster key, so it cannot open the secret. Starting
    /// the container anyway would give it an empty or encrypted file.
    #[error("no cluster key on this node, cannot mount secret {name} for {instance}")]
    NoSecretKey { name: String, instance: String },

    #[error("secret {name} was sealed with key {expected}, this node holds {got}")]
    WrongSecretKey {
        name: String,
        expected: String,
        got: String,
    },

    #[error("{path}: {source}")]
    ObjectIo {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("docker: {0}")]
    Docker(#[from] cl_docker::DockerError),

    /// Boxed because openraft's storage error is 224 bytes and would otherwise
    /// bloat every Result in this crate.
    #[error("cluster: {0}")]
    Cluster(#[from] Box<cl_raft::ClusterError>),
}

impl From<cl_raft::ClusterError> for SchedError {
    fn from(e: cl_raft::ClusterError) -> Self {
        SchedError::Cluster(Box::new(e))
    }
}
