/// Txpool error.
#[derive(thiserror::Error, Debug)]
pub enum TxPoolError {
    #[error("{0}")]
    Fjall(#[from] fjall::Error),
    #[error("database format version mismatch: this binary supports format version {expected} but the on-disk database is version {found}; refusing to start (a future release may provide a migration)")]
    DbFormatVersionMismatch { expected: u64, found: u64 },
    #[error("Required key not found")]
    NotFound,
}
