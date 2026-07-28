pub type DbResult<T> = Result<T, BlockchainError>;

/// A blockchain error.
#[derive(thiserror::Error, Debug)]
pub enum BlockchainError {
    #[error(transparent)]
    IO(#[from] std::io::Error),
    #[error(transparent)]
    Fjall(#[from] fjall::Error),
    #[error("database is corrupt: {0}")]
    Corrupt(&'static str),
    #[error("not found")]
    NotFound,
}
