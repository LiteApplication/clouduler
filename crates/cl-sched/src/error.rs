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

    #[error("docker: {0}")]
    Docker(#[from] cl_docker::DockerError),

    #[error("cluster: {0}")]
    Cluster(#[from] cl_raft::ClusterError),
}
