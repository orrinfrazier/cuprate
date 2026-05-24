use std::{collections::HashMap, sync::Mutex};

use fjall::{KeyspaceCreateOptions, KvSeparationOptions};

use crate::error::TxPoolError;

/// Current on-disk format version of the txpool database.
///
/// Incremented by 1 whenever the txpool database structure/schema changes.
/// Persisted under `b"format_version"` in the `metadata` keyspace and checked on open.
const DATABASE_FORMAT_VERSION: u64 = 0;

/// The txpool database.
pub struct TxpoolDatabase {
    pub(crate) fjall_database: fjall::Database,

    pub(crate) tx_blobs: fjall::Keyspace,
    pub(crate) tx_infos: fjall::Keyspace,
    pub(crate) spent_key_images: fjall::Keyspace,
    pub(crate) known_blob_hashes: fjall::Keyspace,
    pub(crate) metadata: fjall::Keyspace,

    pub(crate) in_progress_key_images: Mutex<HashMap<[u8; 32], [u8; 32]>>,
}

impl TxpoolDatabase {
    /// Open a txpool database with the given fjall backing database.
    pub fn open_with_database(fjall_database: fjall::Database) -> Result<Self, TxPoolError> {
        let s = Self {
            tx_blobs: fjall_database.keyspace("tx_blobs", || {
                KeyspaceCreateOptions::default().with_kv_separation(Some(
                    KvSeparationOptions::default().separation_threshold(3_000),
                ))
            })?,
            tx_infos: fjall_database.keyspace("tx_infos", KeyspaceCreateOptions::default)?,
            spent_key_images: fjall_database
                .keyspace("spent_key_images", KeyspaceCreateOptions::default)?,
            known_blob_hashes: fjall_database
                .keyspace("known_blob_hashes", KeyspaceCreateOptions::default)?,
            metadata: fjall_database.keyspace("metadata", KeyspaceCreateOptions::default)?,
            fjall_database,
            in_progress_key_images: Mutex::new(HashMap::new()),
        };

        Self::check_or_init_format_version(&s.metadata)?;

        Ok(s)
    }

    /// Reads `b"format_version"` from `metadata`. If absent, stamps `DATABASE_FORMAT_VERSION`.
    /// If present and equal, ok. Otherwise returns `DbFormatVersionMismatch` (never panics).
    fn check_or_init_format_version(metadata: &fjall::Keyspace) -> Result<(), TxPoolError> {
        match metadata.get(b"format_version")? {
            None => {
                metadata.insert(b"format_version", DATABASE_FORMAT_VERSION.to_le_bytes())?;
                Ok(())
            }
            Some(bytes) => {
                let mut version_bytes = [0_u8; 8];
                let copy_len = bytes.len().min(version_bytes.len());
                version_bytes[..copy_len].copy_from_slice(&bytes[..copy_len]);

                let found = u64::from_le_bytes(version_bytes);

                if found == DATABASE_FORMAT_VERSION && bytes.len() == version_bytes.len() {
                    Ok(())
                } else {
                    Err(TxPoolError::DbFormatVersionMismatch {
                        expected: DATABASE_FORMAT_VERSION,
                        found,
                    })
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{TxpoolDatabase, DATABASE_FORMAT_VERSION};
    use crate::error::TxPoolError;

    #[test]
    fn open_with_database_stamps_format_version_on_fresh_db() {
        let dir = tempfile::tempdir().unwrap();
        let fjall = fjall::Database::builder(dir.path()).open().unwrap();
        let db = TxpoolDatabase::open_with_database(fjall).unwrap();

        let stored = db.metadata.get(b"format_version").unwrap().unwrap();

        assert_eq!(stored.as_ref(), DATABASE_FORMAT_VERSION.to_le_bytes());
    }

    #[test]
    fn open_with_database_allows_reopen_with_matching_format_version() {
        let dir = tempfile::tempdir().unwrap();
        let fjall = fjall::Database::builder(dir.path()).open().unwrap();
        let db = TxpoolDatabase::open_with_database(fjall).unwrap();

        drop(db);

        let fjall = fjall::Database::builder(dir.path()).open().unwrap();
        let reopened = TxpoolDatabase::open_with_database(fjall);

        assert!(reopened.is_ok());
    }

    #[test]
    fn open_with_database_rejects_mismatched_format_version() {
        let dir = tempfile::tempdir().unwrap();
        let fjall = fjall::Database::builder(dir.path()).open().unwrap();
        let metadata = fjall
            .keyspace("metadata", fjall::KeyspaceCreateOptions::default)
            .unwrap();
        metadata
            .insert(b"format_version", 999_u64.to_le_bytes())
            .unwrap();

        match TxpoolDatabase::open_with_database(fjall) {
            Err(TxPoolError::DbFormatVersionMismatch { expected, found }) => {
                assert_eq!(expected, DATABASE_FORMAT_VERSION);
                assert_eq!(found, 999);
            }
            Err(other) => panic!("unexpected error: {other:?}"),
            Ok(_) => panic!("expected format version mismatch error"),
        }
    }

    #[test]
    fn open_with_database_rejects_malformed_format_version_value() {
        let dir = tempfile::tempdir().unwrap();
        let fjall = fjall::Database::builder(dir.path()).open().unwrap();
        let metadata = fjall
            .keyspace("metadata", fjall::KeyspaceCreateOptions::default)
            .unwrap();
        metadata.insert(b"format_version", [1_u8, 2, 3]).unwrap();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            TxpoolDatabase::open_with_database(fjall)
        }));

        assert!(
            result.is_ok(),
            "open_with_database panicked on malformed version"
        );
        assert!(result.unwrap().is_err());
    }
}
