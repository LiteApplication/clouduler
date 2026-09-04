use thiserror::Error;

#[derive(Debug, Error)]
pub enum ResticError {
    #[error("running restic: {0}")]
    Io(#[from] std::io::Error),
    #[error("restic exited with an error: {0}")]
    Failed(String),
    #[error("could not parse restic output: {0}")]
    Parse(String),
}

pub type Result<T> = std::result::Result<T, ResticError>;
