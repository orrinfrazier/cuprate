//! Roundtrip tests for the blockchain database ops.
//!
//! Two complementary harnesses are used because the `block_infos`/`tx_infos` tapes are
//! *positional* (tape index == block height):
//!
//! * **Real fixtures** ([`mock_block`] is not used) exercise transaction/output blob fidelity
//!   that is height-independent — `get_tx` (real pruned + prunable reassembly for V1 and V2),
//!   output lookups, and `get_block` via a captured [`BlockInfo`]. Real mainnet fixtures live at
//!   non-contiguous heights, so they cannot be round-tripped *by height* in an empty DB.
//! * **A synthetic genesis chain** ([`mock_block`], miner-only V2 blocks at heights `0..N`)
//!   exercises the natural height-positional lifecycle — `get_block_by_height`/`by_hash`,
//!   `get_block_complete_entry`, `chain_height`/`top_block_height`, and `pop_block`.

use std::borrow::Cow;

use fjall::PersistMode;
use monero_oxide::{
    block::{Block, BlockHeader},
    ed25519::CompressedPoint,
    transaction::{Input, Output, Timelock, Transaction, TransactionPrefix},
};
use tapes::{Persistence, TapesRead};

use cuprate_test_utils::data::{BLOCK_V16_TX0, BLOCK_V1_TX2, BLOCK_V9_TX3};
use cuprate_types::VerifiedBlockInformation;

use crate::{
    config::Config,
    error::BlockchainError,
    ops::{
        block::{
            add_block_to_dynamic_tables, add_blocks_to_tapes, block_exists, get_block,
            get_block_by_hash, get_block_complete_entry_from_height, get_block_height, pop_block,
        },
        blockchain::{chain_height, top_block_height},
        output::{get_num_outputs_with_amount, get_output, id_to_output_on_chain},
        tx::{get_num_tx, get_tx, tx_exists},
    },
    types::PreRctOutputId,
    BlockchainDatabase,
};

//---------------------------------------------------------------------------------------------------- Harness

/// Open a fresh blockchain database backed by a temporary directory.
fn tmp_db() -> (tempfile::TempDir, BlockchainDatabase) {
    let tempdir = tempfile::tempdir().expect("failed to create tempdir");
    let config = Config {
        blob_dir: tempdir.path().to_path_buf(),
        index_dir: tempdir.path().to_path_buf(),
        ..Default::default()
    };
    let fjall = fjall::Database::builder(tempdir.path())
        .open()
        .expect("failed to open fjall database");
    let db = BlockchainDatabase::open_with_fjall_database(&config, fjall)
        .expect("failed to open blockchain database");
    (tempdir, db)
}

/// Write a single block to the database, replicating `service::write::write_blocks`
/// (tapes append + commit, then fjall dynamic-table batch + commit).
fn write_block(db: &BlockchainDatabase, block: &VerifiedBlockInformation) {
    let blocks = std::slice::from_ref(block);

    let mut tapes = db.linear_tapes.append();
    let mut numb_transactions = tapes
        .fixed_sized_tape_len(&db.tx_infos)
        .expect("tx_infos tape must be open");
    add_blocks_to_tapes(blocks, db, &mut tapes).expect("add_blocks_to_tapes failed");
    tapes
        .commit(Persistence::Buffer)
        .expect("tapes commit failed");

    let mut cache = db.pre_rct_numb_outputs_cache.lock().unwrap();
    let mut tx_rw = db.fjall.batch().durability(Some(PersistMode::Buffer));
    add_block_to_dynamic_tables(
        db,
        &block.block,
        &block.block_hash,
        block.txs.iter().map(|tx| Cow::Borrowed(&tx.tx)),
        &mut numb_transactions,
        &mut tx_rw,
        &mut cache,
    )
    .expect("add_block_to_dynamic_tables failed");
    tx_rw.commit().expect("fjall batch commit failed");
}

/// Build a synthetic miner-only V2 block at `height` extending `previous`.
///
/// This mirrors the block generation used by the `cuprated` reorg tests and needs no
/// consensus machinery — the storage layer does not validate block rewards.
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
    .expect("failed to build synthetic block");

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

//---------------------------------------------------------------------------------------------------- Piece B1: block read roundtrip (real fixtures)

/// Writing a real fixture block and reading it back (via its captured [`BlockInfo`])
/// yields a byte-identical block. Covers a 0-tx block, a V1 block, and a V2 block.
#[test]
fn fixture_block_roundtrip() {
    for block in [&*BLOCK_V16_TX0, &*BLOCK_V1_TX2, &*BLOCK_V9_TX3] {
        let (_tempdir, db) = tmp_db();
        write_block(&db, block);

        let reader = db.linear_tapes.reader();
        // The block info is at tape index 0 (it is the only block written).
        let block_info = reader
            .read_entry(&db.block_infos, 0)
            .expect("read block_infos failed")
            .expect("block info must exist at index 0");

        let got =
            get_block(&block.height, Some(&block_info), &reader, &db).expect("get_block failed");

        assert_eq!(
            got.serialize(),
            block.block.serialize(),
            "block at height {} did not round-trip",
            block.height,
        );
        assert_eq!(got.hash(), block.block_hash, "block hash mismatch");
    }
}

//---------------------------------------------------------------------------------------------------- Piece B2: tx + output roundtrip (real fixtures)

/// Each transaction in a real V1 fixture block round-trips through `get_tx`
/// (exercising the v1-prunable reassembly path), and its pre-RCT outputs are retrievable.
#[test]
fn fixture_v1_tx_and_output_roundtrip() {
    let (_tempdir, db) = tmp_db();
    let block = &*BLOCK_V1_TX2;
    write_block(&db, block);

    let reader = db.linear_tapes.reader();
    let snapshot = db.fjall.snapshot();

    // tx_ids contains the miner tx plus every block tx.
    assert_eq!(
        get_num_tx(&db, &snapshot).expect("get_num_tx failed"),
        (block.txs.len() + 1) as u64,
        "unexpected stored transaction count",
    );

    for tx in &block.txs {
        assert!(
            tx_exists(&db, &tx.tx_hash, &snapshot).expect("tx_exists failed"),
            "tx should exist after write",
        );

        let got = get_tx(&db, &tx.tx_hash, &snapshot, &reader).expect("get_tx failed");
        let expected_blob = [tx.tx_pruned.as_slice(), tx.tx_prunable_blob.as_slice()].concat();
        assert_eq!(got.serialize(), expected_blob, "tx blob did not round-trip");
        assert_eq!(got.hash(), tx.tx_hash, "tx hash mismatch");
    }

    // Pre-RCT output lookups: the boundary index is retrievable and one-past-the-end is absent.
    let amount = block.txs[0].tx.prefix().outputs[0]
        .amount
        .expect("V1 output must have a clear amount");
    let count = get_num_outputs_with_amount(&db, &snapshot, amount)
        .expect("get_num_outputs_with_amount failed");
    assert!(
        count >= 1,
        "expected at least one output for amount {amount}"
    );

    get_output(
        &db,
        &PreRctOutputId {
            amount,
            amount_index: count - 1,
        },
        &snapshot,
    )
    .expect("last output of an amount must be retrievable");

    assert!(
        matches!(
            get_output(
                &db,
                &PreRctOutputId {
                    amount,
                    amount_index: count,
                },
                &snapshot,
            ),
            Err(BlockchainError::NotFound)
        ),
        "one-past-the-end output index must be absent",
    );
}

/// Each transaction in a real V2 fixture block round-trips through `get_tx`
/// (exercising the pruning-stripe reassembly path), and an RCT output is retrievable on-chain.
#[test]
fn fixture_v2_tx_and_output_roundtrip() {
    let (_tempdir, db) = tmp_db();
    let block = &*BLOCK_V9_TX3;
    write_block(&db, block);

    let reader = db.linear_tapes.reader();
    let snapshot = db.fjall.snapshot();

    for tx in &block.txs {
        let got = get_tx(&db, &tx.tx_hash, &snapshot, &reader).expect("get_tx failed");
        let expected_blob = [tx.tx_pruned.as_slice(), tx.tx_prunable_blob.as_slice()].concat();
        assert_eq!(got.serialize(), expected_blob, "tx blob did not round-trip");
        assert_eq!(got.hash(), tx.tx_hash, "tx hash mismatch");
    }

    // The first RCT output (amount 0, index 0) is the miner tx's first output; it must map to an
    // on-chain output whose resolved txid is exactly the miner transaction's hash.
    let output = id_to_output_on_chain(
        &db,
        &PreRctOutputId {
            amount: 0,
            amount_index: 0,
        },
        true,
        &snapshot,
        &reader,
    )
    .expect("first RCT output must be retrievable");
    assert_eq!(
        output.txid,
        Some(block.block.miner_transaction().hash()),
        "first RCT output should resolve to the miner transaction",
    );
}

//---------------------------------------------------------------------------------------------------- Piece B3: synthetic genesis chain lifecycle

/// A synthetic two-block chain supports the natural height-keyed read lifecycle:
/// height/hash lookups, complete-entry retrieval (pruned + non-pruned), and `pop_block`.
#[test]
fn synthetic_chain_lifecycle() {
    let (_tempdir, db) = tmp_db();

    let block0 = mock_block(0, [0; 32]);
    write_block(&db, &block0);
    let block1 = mock_block(1, block0.block_hash);
    write_block(&db, &block1);

    {
        let reader = db.linear_tapes.reader();
        let snapshot = db.fjall.snapshot();

        assert_eq!(chain_height(&db, &reader).expect("chain_height failed"), 2);
        assert_eq!(
            top_block_height(&db, &reader).expect("top_block_height failed"),
            1
        );

        // Read by height.
        assert_eq!(
            get_block(&0, None, &reader, &db)
                .expect("get_block(0) failed")
                .serialize(),
            block0.block.serialize(),
        );
        assert_eq!(
            get_block(&1, None, &reader, &db)
                .expect("get_block(1) failed")
                .serialize(),
            block1.block.serialize(),
        );

        // Read by hash.
        assert_eq!(
            get_block_by_hash(&db, &block0.block_hash, &snapshot, &reader)
                .expect("get_block_by_hash failed")
                .hash(),
            block0.block_hash,
        );
        assert_eq!(
            get_block_height(&db, &block1.block_hash, &snapshot).expect("get_block_height failed"),
            1,
        );
        assert!(block_exists(&db, &block0.block_hash, &snapshot).expect("block_exists failed"));

        // Complete entry: both heights, pruned and non-pruned.
        for height in [0, 1] {
            let block_blob = if height == 0 {
                &block0.block_blob
            } else {
                &block1.block_blob
            };
            for pruned in [true, false] {
                let entry = get_block_complete_entry_from_height(height, pruned, &reader, &db)
                    .expect("get_block_complete_entry_from_height failed");
                assert_eq!(entry.pruned, pruned, "pruned flag mismatch");
                assert_eq!(
                    entry.block.as_ref(),
                    block_blob.as_slice(),
                    "complete-entry block blob mismatch at height {height} (pruned={pruned})",
                );
            }
        }
    }

    // Pop the top block and verify prior state is restored.
    {
        let mut truncate = db.linear_tapes.truncate();
        let mut tx_rw = db.fjall.batch();
        let (popped_height, popped_hash, popped_block) =
            pop_block(&db, None, &mut tx_rw, &mut truncate).expect("pop_block failed");
        tx_rw.commit().expect("fjall commit failed");
        truncate
            .commit(Persistence::SyncAll)
            .expect("tapes truncate commit failed");

        assert_eq!(popped_height, 1, "popped wrong height");
        assert_eq!(popped_hash, block1.block_hash, "popped wrong hash");
        assert_eq!(
            popped_block.serialize(),
            block1.block.serialize(),
            "popped block bytes mismatch",
        );
    }

    let reader = db.linear_tapes.reader();
    let snapshot = db.fjall.snapshot();

    assert_eq!(
        chain_height(&db, &reader).expect("chain_height after pop failed"),
        1,
        "chain height should be restored to 1 after pop",
    );
    assert!(
        !block_exists(&db, &block1.block_hash, &snapshot).expect("block_exists failed"),
        "popped block must be removed from block_heights",
    );
    assert!(
        block_exists(&db, &block0.block_hash, &snapshot).expect("block_exists failed"),
        "block 0 must survive the pop",
    );
    assert_eq!(
        get_block(&0, None, &reader, &db)
            .expect("get_block(0) after pop failed")
            .serialize(),
        block0.block.serialize(),
        "block 0 must remain readable after pop",
    );
}
