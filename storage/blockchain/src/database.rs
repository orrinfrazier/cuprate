use std::{borrow::Cow, collections::HashMap, sync::Mutex};

use fjall::{KeyspaceCreateOptions, PersistMode, Readable};
use monero_oxide::transaction::{Pruned, Transaction};
use tapes::{Persistence, TapeOpenOptions, Tapes, TapesRead};

use cuprate_helper::cast::{u64_to_usize, usize_to_u64};

use crate::{
    config::Config,
    error::DbResult,
    types::{Amount, BlockInfo, RctOutput, TxInfo},
    BlockchainError,
};

/// The blockchain database.
pub struct BlockchainDatabase {
    /// The tapes database.
    pub(crate) linear_tapes: Tapes,
    /// The fjall database.
    pub(crate) fjall: fjall::Database,

    /// Block heights:
    ///
    /// | key                  | value                               |
    /// |----------------------|-------------------------------------|
    /// | block hash: [u8; 32] | block height: usize (little endian) |
    pub(crate) block_heights: fjall::Keyspace,
    /// Key images:
    ///
    /// | key                 | value |
    /// |---------------------|-------|
    /// | key image: [u8; 32] | []    |
    pub(crate) key_images: fjall::Keyspace,
    /// Pre-RCT outputs:
    ///
    /// | key                                     | value                             |
    /// |-----------------------------------------|-----------------------------------|
    /// | The ID of the output [`PreRctOutputId`] | The output data: [`Output`] bytes |
    pub(crate) pre_rct_outputs: fjall::Keyspace,
    /// Transaction IDs:
    ///
    /// | key               | value                      |
    /// |-------------------|----------------------------|
    /// | Tx hash: [u8; 32] | Tx ID: u64 (little endian) |
    pub(crate) tx_ids: fjall::Keyspace,
    /// V1 transaction output amount indices:
    ///
    /// | key                        | value                                           |
    /// |----------------------------|--------------------------------------------------|
    /// | Tx ID: u64 (little endian) | amount indices as a [u64] (little endian) slice |
    pub(crate) v1_tx_outputs: fjall::Keyspace,
    /// Alt chain info:
    ///
    /// | key                           | value                  |
    /// |-------------------------------|------------------------|
    /// | Chain ID: u64 (little endian) | [`AltChainInfo`] bytes |
    pub(crate) alt_chain_infos: fjall::Keyspace,
    /// Alt block heights:
    ///
    /// | key                  | value                    |
    /// |----------------------|--------------------------|
    /// | block hash: [u8; 32] | [`AltBlockHeight`] bytes |
    pub(crate) alt_block_heights: fjall::Keyspace,
    /// Alt block info:
    ///
    /// | key                        | value                          |
    /// |----------------------------|--------------------------------|
    /// | [`AltBlockHeight`] bytes   | [`CompactAltBlockInfo`] bytes  |
    pub(crate) alt_block_infos: fjall::Keyspace,
    /// Alt block blobs:
    ///
    /// | key                      | value            |
    /// |--------------------------|------------------|
    /// | [`AltBlockHeight`] bytes | block blob: [u8] |
    pub(crate) alt_block_blobs: fjall::Keyspace,
    /// Alt transaction blobs:
    ///
    /// | key                        | value                       |
    /// |----------------------------|-----------------------------|
    /// | transaction hash: [u8; 32] | full transaction blob: [u8] |
    pub(crate) alt_transaction_blobs: fjall::Keyspace,
    /// Alt transaction info:
    ///
    /// | key                        | value                        |
    /// |----------------------------|------------------------------|
    /// | transaction hash: [u8; 32] | [`AltTransactionInfo`] bytes |
    pub(crate) alt_transaction_infos: fjall::Keyspace,

    /// RCT (v2+) outputs, indexed sequentially.
    ///
    /// | index                 | value         |
    /// |-----------------------|---------------|
    /// | RCT output index: u64 | [`RctOutput`] |
    pub(crate) rct_outputs: tapes::FixedSizedTape<RctOutput>,
    /// Transaction info, indexed by [`TxId`].
    ///
    /// | index      | value      |
    /// |------------|------------|
    /// | Tx ID: u64 | [`TxInfo`] |
    pub(crate) tx_infos: tapes::FixedSizedTape<TxInfo>,
    /// Block info, indexed by block height.
    ///
    /// | index             | value         |
    /// |-------------------|---------------|
    /// | Block height: u64 | [`BlockInfo`] |
    pub(crate) block_infos: tapes::FixedSizedTape<BlockInfo>,
    /// Pruned blobs.
    ///
    /// The format for this blob-tape per each block is:
    ///
    /// | data                                       |
    /// |--------------------------------------------|
    /// | block blob (header, miner tx, tx hashes)   |
    /// | tx 0 pruned blob                           |
    /// | tx 0 prunable hash (32 bytes)              |
    /// | tx 1 pruned blob                           |
    /// | tx 1 prunable hash (32 bytes)              |
    /// | ...                                        |
    ///
    /// The prunable hash is `[0; 32]` for v1 txs.
    /// Each block is appended directly after the one before it.
    pub(crate) pruned_blobs: tapes::BlobTape,
    /// V1 prunable transaction blobs, indexed by [`TxInfo::prunable_blob_idx`].
    ///
    /// This tape stores the prunable blob for all V1 txs, these can't be pruned.
    pub(crate) v1_prunable_blobs: tapes::BlobTape,
    /// V2+ prunable transaction blobs, split across 8 stripes.
    /// Indexed by [`TxInfo::prunable_blob_idx`].
    ///
    /// These tapes store the prunable part of each tx, the stripe a tx is stored in depends on the
    /// height of the block.
    pub(crate) prunable_blobs: Vec<tapes::BlobTape>,

    /// A runtime cache of the number of outputs for each pre-rct output amount.
    /// This is filled in lazily.
    pub(crate) pre_rct_numb_outputs_cache: Mutex<HashMap<Amount, u64>>,
}

impl BlockchainDatabase {
    /// Open a [`BlockchainDatabase`] with an [`fjall::Database`] for storing data that can't be stored in tapes.
    pub fn open_with_fjall_database(
        config: &Config,
        fjall: fjall::Database,
    ) -> Result<Self, BlockchainError> {
        let block_heights = fjall.keyspace("block_heights", KeyspaceCreateOptions::default)?;
        let key_images = fjall.keyspace("key_images", KeyspaceCreateOptions::default)?;
        let pre_rct_outputs = fjall.keyspace("pre_rct_outputs", KeyspaceCreateOptions::default)?;
        let tx_ids = fjall.keyspace("tx_ids", KeyspaceCreateOptions::default)?;
        let v1_tx_outputs = fjall.keyspace("tx_outputs", KeyspaceCreateOptions::default)?;

        let alt_chain_infos = fjall.keyspace("alt_chain_infos", KeyspaceCreateOptions::default)?;
        let alt_block_heights =
            fjall.keyspace("alt_block_heights", KeyspaceCreateOptions::default)?;
        let alt_block_infos = fjall.keyspace("alt_block_infos", KeyspaceCreateOptions::default)?;
        let alt_block_blobs = fjall.keyspace("alt_block_blobs", KeyspaceCreateOptions::default)?;
        let alt_transaction_blobs =
            fjall.keyspace("alt_transaction_blobs", KeyspaceCreateOptions::default)?;
        let alt_transaction_infos =
            fjall.keyspace("alt_transaction_infos", KeyspaceCreateOptions::default)?;

        let tapes_index_dir = config.index_dir.join("tapes");
        let tapes_blob_dir = config.blob_dir.join("tapes");

        let linear_tapes = Tapes::open(&tapes_index_dir)?;
        let mut tape_append_tx = linear_tapes.append();

        let rct_outputs = tape_append_tx.open_fixed_sized_tape(
            "rct_outputs",
            &TapeOpenOptions {
                top_cache_size: config.cache_sizes.rct_outputs,
                dir: tapes_index_dir.clone(),
            },
        )?;
        let tx_infos = tape_append_tx.open_fixed_sized_tape(
            "tx_infos",
            &TapeOpenOptions {
                top_cache_size: config.cache_sizes.tx_infos,
                dir: tapes_index_dir.clone(),
            },
        )?;
        let block_infos = tape_append_tx.open_fixed_sized_tape(
            "block_infos",
            &TapeOpenOptions {
                top_cache_size: config.cache_sizes.block_infos,
                dir: tapes_index_dir,
            },
        )?;
        let pruned_blobs = tape_append_tx.open_blob_tape(
            "pruned_blobs",
            &TapeOpenOptions {
                top_cache_size: config.cache_sizes.pruned_blobs,
                dir: tapes_blob_dir.clone(),
            },
        )?;
        let v1_prunable_blobs = tape_append_tx.open_blob_tape(
            "v1_prunable_blobs",
            &TapeOpenOptions {
                top_cache_size: config.cache_sizes.v1_prunable_blobs,
                dir: tapes_blob_dir.clone(),
            },
        )?;

        const PRUNABLE_BLOBS: [&str; 8] = [
            "prunable1",
            "prunable2",
            "prunable3",
            "prunable4",
            "prunable5",
            "prunable6",
            "prunable7",
            "prunable8",
        ];

        let prunable_blobs = (0..8)
            .map(|i| {
                tape_append_tx.open_blob_tape(
                    PRUNABLE_BLOBS[i],
                    &TapeOpenOptions {
                        top_cache_size: config.cache_sizes.prunable_blobs,
                        dir: tapes_blob_dir.clone(),
                    },
                )
            })
            .collect::<Result<_, _>>()?;

        tape_append_tx.commit(Persistence::SyncAll)?;

        drop(tape_append_tx);

        tracing::debug!("opened db");
        Ok(Self {
            fjall,
            linear_tapes,
            block_heights,
            key_images,
            pre_rct_outputs,
            tx_ids,
            v1_tx_outputs,
            alt_chain_infos,
            alt_block_heights,
            alt_block_infos,
            alt_block_blobs,
            alt_transaction_blobs,
            alt_transaction_infos,
            rct_outputs,
            tx_infos,
            block_infos,
            pruned_blobs,
            v1_prunable_blobs,
            prunable_blobs,
            pre_rct_numb_outputs_cache: Mutex::new(HashMap::new()),
        })
    }

    /// Checks if the fjall and tapes database are in sync and rebuilds the fjall database if it
    /// is not.
    pub fn make_consistent(&self) -> Result<(), BlockchainError> {
        tracing::info!("Checking blockchain database consistency.");

        let tapes_reader = self.linear_tapes.reader();
        let block_infos_len = tapes_reader
            .fixed_sized_tape_len(&self.block_infos)
            .ok_or(BlockchainError::Corrupt("block_infos tape missing"))?;
        let block_heights_len = usize_to_u64(self.block_heights.len()?);

        if block_infos_len != block_heights_len {
            tracing::warn!("fjall and tapes are out of sync");
            return self.rebuild_fjall_database();
        }

        let tx_infos_len = tapes_reader
            .fixed_sized_tape_len(&self.tx_infos)
            .ok_or(BlockchainError::Corrupt("tx_infos tape missing"))?;
        let tx_ids_len = usize_to_u64(self.tx_ids.len()?);

        if tx_infos_len != tx_ids_len {
            tracing::warn!("fjall tx_ids and tx_infos are out of sync");
            return self.rebuild_fjall_database();
        }

        if block_infos_len == 0 {
            return Ok(());
        }

        let top_height = u64_to_usize(block_infos_len - 1);
        let Some(top_block_info) =
            tapes_reader.read_entry(&self.block_infos, block_infos_len - 1)?
        else {
            return Err(BlockchainError::Corrupt("block_infos tail entry missing"));
        };

        let snapshot = self.fjall.snapshot();
        let Some(stored_height) = snapshot.get(&self.block_heights, top_block_info.block_hash)?
        else {
            tracing::warn!("top block hash missing from block_heights");
            return self.rebuild_fjall_database();
        };

        let stored_height = u64_to_usize(u64::from_le_bytes(
            stored_height
                .as_ref()
                .try_into()
                .map_err(|_| BlockchainError::Corrupt("block_heights entry has invalid length"))?,
        ));

        if stored_height != top_height {
            tracing::warn!("top block height does not match tail block_info");
            return self.rebuild_fjall_database();
        }

        Ok(())
    }

    /// Rebuilds the fjall database.
    pub fn rebuild_fjall_database(&self) -> Result<(), BlockchainError> {
        self.block_heights.clear()?;
        self.key_images.clear()?;
        self.pre_rct_outputs.clear()?;
        self.tx_ids.clear()?;
        self.v1_tx_outputs.clear()?;
        self.alt_chain_infos.clear()?;
        self.alt_block_heights.clear()?;
        self.alt_block_infos.clear()?;
        self.alt_block_blobs.clear()?;
        self.alt_transaction_blobs.clear()?;
        self.alt_transaction_infos.clear()?;

        let rebuild_span = tracing::info_span!("rebuild_fjall_database");
        let _guard = rebuild_span.enter();

        tracing::info!("rebuilding fjall db");

        let tapes_reader = self.linear_tapes.reader();

        let tx_infos_iter = tapes_reader.iter_from(&self.tx_infos, 0)?;
        let mut tx_iter = tx_infos_iter.map(|tx_info| -> DbResult<Cow<Transaction<Pruned>>> {
            let tx_info = tx_info?;

            let blob_tape_len = tapes_reader
                .blob_tape_len(&self.pruned_blobs)
                .ok_or(BlockchainError::Corrupt("pruned_blobs tape missing"))?;
            if tx_info
                .pruned_blob_idx
                .saturating_add(tx_info.pruned_size as u64)
                > blob_tape_len
            {
                return Err(BlockchainError::Corrupt(
                    "tx pruned blob range out of bounds",
                ));
            }

            let mut tx_blob = vec![0; tx_info.pruned_size];
            tapes_reader.read_bytes(&self.pruned_blobs, tx_info.pruned_blob_idx, &mut tx_blob)?;

            let tx = Transaction::read(&mut tx_blob.as_slice())?;

            Ok(Cow::Owned(tx))
        });

        let mut batch = self.fjall.batch().durability(Some(PersistMode::Buffer));
        let mut numb_txs = 0;
        for height in 0..tapes_reader
            .fixed_sized_tape_len(&self.block_infos)
            .ok_or(BlockchainError::Corrupt("block_infos tape missing"))?
        {
            let block =
                crate::ops::block::get_block(&u64_to_usize(height), None, &tapes_reader, self)?;

            let _miner_tx = tx_iter.next().transpose()?;

            crate::ops::block::add_block_to_dynamic_tables(
                self,
                &block,
                &block.hash(),
                &mut tx_iter,
                &mut numb_txs,
                &mut batch,
                &mut self.pre_rct_numb_outputs_cache.lock().unwrap(),
            )?;

            if height % 1000 == 0 {
                tracing::info!("{} blocks processed", height);
                let old_batch = std::mem::replace(
                    &mut batch,
                    self.fjall.batch().durability(Some(PersistMode::Buffer)),
                );

                old_batch.commit()?;
            }
        }

        batch.commit()?;

        Ok(())
    }
}

impl Drop for BlockchainDatabase {
    fn drop(&mut self) {
        tracing::info!(parent: &tracing::Span::none(), "Syncing blockchain database to storage.");

        let _ = self.fjall.persist(PersistMode::SyncAll);

        let _ = self.linear_tapes.append().commit(Persistence::SyncAll);
    }
}

#[cfg(test)]
mod tests {
    use std::{
        borrow::Cow,
        collections::HashMap,
        fs,
        path::{Path, PathBuf},
    };

    use cuprate_helper::cast::usize_to_u64;
    use cuprate_types::VerifiedBlockInformation;
    use fjall::{PersistMode, Readable};
    use monero_oxide::{
        block::{Block, BlockHeader},
        ed25519::CompressedPoint,
        transaction::{Input, Output, Pruned, Timelock, Transaction, TransactionPrefix},
    };
    use tapes::{Persistence, TapesRead};

    use super::BlockchainDatabase;
    use crate::{
        config::Config,
        error::BlockchainError,
        ops::block::{add_block_to_dynamic_tables, add_blocks_to_tapes, get_block},
    };

    fn tmp_db() -> (tempfile::TempDir, BlockchainDatabase) {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config = Config {
            blob_dir: tempdir.path().to_path_buf(),
            index_dir: tempdir.path().to_path_buf(),
            ..Default::default()
        };
        let fjall = fjall::Database::builder(tempdir.path())
            .open()
            .expect("fjall open");
        let db = BlockchainDatabase::open_with_fjall_database(&config, fjall).expect("db open");
        (tempdir, db)
    }

    fn mock_block(height: usize, previous: [u8; 32]) -> VerifiedBlockInformation {
        let block = Block::new(
            BlockHeader {
                hardfork_version: 16,
                hardfork_signal: 16,
                timestamp: 1_000 + height as u64,
                previous,
                nonce: 0,
            },
            Transaction::V2 {
                prefix: TransactionPrefix {
                    additional_timelock: Timelock::Block(height + 60),
                    inputs: vec![Input::Gen(height)],
                    outputs: vec![Output {
                        amount: Some(1_000_000_000),
                        key: CompressedPoint::from([1; 32]),
                        view_tag: Some(1),
                    }],
                    extra: vec![],
                },
                proofs: None,
            },
            vec![],
        )
        .expect("build block");
        let block_hash = block.hash();
        let block_blob = block.serialize();
        VerifiedBlockInformation {
            block_hash,
            block_blob,
            block,
            txs: vec![],
            pow_hash: [0; 32],
            height,
            generated_coins: 1_000_000_000,
            weight: 0,
            long_term_weight: 0,
            cumulative_difficulty: 1,
        }
    }

    fn write_block(db: &BlockchainDatabase, block: &VerifiedBlockInformation) {
        let blocks = std::slice::from_ref(block);
        let mut tapes = db.linear_tapes.append();
        let mut numb = tapes
            .fixed_sized_tape_len(&db.tx_infos)
            .expect("tx_infos open");
        add_blocks_to_tapes(blocks, db, &mut tapes).expect("add_blocks_to_tapes");
        tapes.commit(Persistence::Buffer).expect("tapes commit");
        let mut cache = db.pre_rct_numb_outputs_cache.lock().unwrap();
        let mut tx_rw = db.fjall.batch().durability(Some(PersistMode::Buffer));
        add_block_to_dynamic_tables(
            db,
            &block.block,
            &block.block_hash,
            block.txs.iter().map(|tx| Ok(Cow::Borrowed(&tx.tx))),
            &mut numb,
            &mut tx_rw,
            &mut cache,
        )
        .expect("add_block_to_dynamic_tables");
        tx_rw.commit().expect("fjall commit");
    }

    fn tx_infos_len(db: &BlockchainDatabase) -> u64 {
        db.linear_tapes
            .reader()
            .fixed_sized_tape_len(&db.tx_infos)
            .expect("tx_infos open")
    }

    fn find_file_named(dir: &Path, needle: &str) -> Option<PathBuf> {
        for entry in fs::read_dir(dir).ok()? {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.is_dir() {
                if let Some(found) = find_file_named(&path, needle) {
                    return Some(found);
                }
            } else if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains(needle))
            {
                return Some(path);
            }
        }

        None
    }

    #[test]
    fn add_block_to_dynamic_tables_propagates_err() {
        let (_tempdir, db) = tmp_db();
        let block = &*cuprate_test_utils::data::BLOCK_V1_TX2;
        let mut numb_transactions = 0;
        let mut tx_rw = db.fjall.batch().durability(Some(PersistMode::Buffer));
        let mut cache = HashMap::new();

        let result = add_block_to_dynamic_tables(
            &db,
            &block.block,
            &block.block_hash,
            std::iter::once(Err::<Cow<Transaction<Pruned>>, BlockchainError>(
                BlockchainError::Corrupt("boom"),
            )),
            &mut numb_transactions,
            &mut tx_rw,
            &mut cache,
        );

        assert!(result.is_err(), "expected iterator error to be returned");
    }

    #[test]
    fn rebuild_returns_err_on_corrupt_tape() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config = Config {
            blob_dir: tempdir.path().to_path_buf(),
            index_dir: tempdir.path().to_path_buf(),
            ..Default::default()
        };

        {
            let fjall = fjall::Database::builder(tempdir.path())
                .open()
                .expect("fjall open");
            let db = BlockchainDatabase::open_with_fjall_database(&config, fjall).expect("db open");

            let mut previous = [0; 32];
            for height in 0..3 {
                let block = mock_block(height, previous);
                previous = block.block_hash;
                write_block(&db, &block);
            }
        }

        let pruned_blobs = find_file_named(&tempdir.path().join("tapes"), "pruned_blobs")
            .expect("pruned_blobs tape file");
        fs::write(&pruned_blobs, b"corrupt").expect("corrupt pruned_blobs");

        let reopen_result = fjall::Database::builder(tempdir.path())
            .open()
            .map_err(BlockchainError::from)
            .and_then(|fjall| BlockchainDatabase::open_with_fjall_database(&config, fjall))
            .and_then(|db| {
                db.block_heights.clear()?;
                db.make_consistent()
            });

        assert!(
            reopen_result.is_err(),
            "rebuild should return Err rather than panic on corrupt tape data"
        );
    }

    #[test]
    fn make_consistent_detects_txid_mismatch() {
        let (_tempdir, db) = tmp_db();

        let mut previous = [0; 32];
        for height in 0..3 {
            let block = mock_block(height, previous);
            previous = block.block_hash;
            write_block(&db, &block);
        }

        let tx_infos_len = tx_infos_len(&db);
        assert_eq!(
            usize_to_u64(db.tx_ids.len().expect("tx_ids len")),
            tx_infos_len
        );

        db.tx_ids.clear().expect("clear tx_ids");
        db.make_consistent()
            .expect("make_consistent should repair tx_ids");

        assert_eq!(
            usize_to_u64(db.tx_ids.len().expect("tx_ids len")),
            tx_infos_len
        );
    }

    #[test]
    fn make_consistent_noop_on_consistent_db() {
        let (_tempdir, db) = tmp_db();

        let mut previous = [0; 32];
        let mut last_block = None;
        for height in 0..3 {
            let block = mock_block(height, previous);
            previous = block.block_hash;
            write_block(&db, &block);
            last_block = Some(block);
        }
        let last_block = last_block.expect("last block");

        let tx_infos_len_before = tx_infos_len(&db);
        let tx_ids_len_before = db.tx_ids.len().expect("tx_ids len");
        let block_heights_len_before = db.block_heights.len().expect("block_heights len");

        db.make_consistent()
            .expect("consistent db should remain valid");

        assert_eq!(tx_infos_len(&db), tx_infos_len_before);
        assert_eq!(db.tx_ids.len().expect("tx_ids len"), tx_ids_len_before);
        assert_eq!(
            db.block_heights.len().expect("block_heights len"),
            block_heights_len_before
        );

        let snapshot = db.fjall.snapshot();
        let stored_height = snapshot
            .get(&db.block_heights, last_block.block_hash)
            .expect("read block height")
            .expect("last block height exists");
        assert_eq!(
            usize::from_le_bytes(stored_height.as_ref().try_into().expect("height bytes")),
            last_block.height
        );

        let restored_block = get_block(&last_block.height, None, &db.linear_tapes.reader(), &db)
            .expect("read block after make_consistent");
        assert_eq!(restored_block.hash(), last_block.block_hash);
    }
}
