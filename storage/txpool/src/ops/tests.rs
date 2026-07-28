//! Roundtrip tests for the tx-pool database ops.
//!
//! These open a temporary fjall database and exercise the real read/write
//! ops (`add_transaction`, `get_transaction_verification_data`,
//! `remove_transaction`, key-image double-spend rejection) against the real
//! transaction fixtures in `cuprate-test-utils`.

use cuprate_test_utils::data::{TX_V1_SIG2, TX_V2_RCT3};
use cuprate_types::{CachedVerificationState, TransactionVerificationData};

use crate::{
    ops::{
        add_transaction, get_transaction_verification_data, in_stem_pool, remove_transaction,
        TxPoolWriteError,
    },
    txpool::TxpoolDatabase,
    TxPoolError,
};

/// Open a fresh tx-pool database backed by a temporary directory.
fn tmp_db() -> (tempfile::TempDir, TxpoolDatabase) {
    let tempdir = tempfile::tempdir().expect("failed to create tempdir");
    let fjall = fjall::Database::builder(tempdir.path())
        .open()
        .expect("failed to open fjall database");
    let db = TxpoolDatabase::open_with_database(fjall).expect("failed to open txpool database");
    (tempdir, db)
}

/// Build a [`TransactionVerificationData`] from a fixture (`cached_verification_state`
/// is [`CachedVerificationState::NotVerified`]).
fn tvd_v1() -> TransactionVerificationData {
    TransactionVerificationData::try_from(TX_V1_SIG2.clone())
        .expect("V1 fixture is valid verification data")
}

fn tvd_v2() -> TransactionVerificationData {
    TransactionVerificationData::try_from(TX_V2_RCT3.clone())
        .expect("V2 fixture is valid verification data")
}

/// Piece T1: `add_transaction` then `get_transaction_verification_data` returns identical data.
#[test]
fn add_then_get_roundtrip() {
    for (tx, state_stem) in [(tvd_v1(), false), (tvd_v2(), true)] {
        let (_tempdir, db) = tmp_db();

        let mut w = db.fjall_database.batch();
        add_transaction(&tx, state_stem, &mut w, &db).expect("add_transaction failed");
        w.commit().expect("commit failed");

        let snapshot = db.fjall_database.snapshot();
        let got = get_transaction_verification_data(&tx.tx_hash, &snapshot, &db)
            .expect("get_transaction_verification_data failed");

        assert_eq!(got.tx_hash, tx.tx_hash, "tx_hash mismatch");
        assert_eq!(got.tx_blob, tx.tx_blob, "tx_blob mismatch");
        assert_eq!(got.tx_weight, tx.tx_weight, "tx_weight mismatch");
        assert_eq!(got.fee, tx.fee, "fee mismatch");
        assert_eq!(got.version, tx.version, "version mismatch");
        assert_eq!(
            got.cached_verification_state,
            CachedVerificationState::NotVerified,
            "cached_verification_state should round-trip to NotVerified",
        );
        // The serialized tx itself must be byte-identical after a read.
        assert_eq!(
            got.tx.serialize(),
            tx.tx.serialize(),
            "tx serialize mismatch"
        );

        // `state_stem` must be reflected by `in_stem_pool`.
        assert_eq!(
            in_stem_pool(&tx.tx_hash, &snapshot, &db).expect("in_stem_pool failed"),
            state_stem,
            "in_stem_pool did not reflect the stored state_stem flag",
        );
    }
}

/// Piece T2: adding a transaction whose key image is already spent is rejected.
#[test]
fn double_spend_rejected() {
    let (_tempdir, db) = tmp_db();
    let tx = tvd_v1();

    // First add succeeds and is committed (the key-image check reads the committed keyspace).
    let mut w = db.fjall_database.batch();
    add_transaction(&tx, false, &mut w, &db).expect("first add_transaction failed");
    w.commit().expect("commit failed");

    // Re-adding the same tx must be rejected as a double spend. The reported value is the
    // hash of the transaction already in the pool that spent the colliding key image.
    let mut w = db.fjall_database.batch();
    let err = add_transaction(&tx, false, &mut w, &db)
        .expect_err("re-adding a spent key image must be rejected");

    match err {
        TxPoolWriteError::DoubleSpend(double_spent_tx) => {
            assert_eq!(
                double_spent_tx, tx.tx_hash,
                "double-spend should report the hash of the conflicting tx",
            );
        }
        TxPoolWriteError::TxPool(e) => panic!("expected DoubleSpend, got TxPool({e})"),
    }
}

/// Piece T3: `remove_transaction` deletes the tx and frees its key images for re-insertion.
#[test]
fn remove_then_reinsert_roundtrip() {
    let (_tempdir, db) = tmp_db();
    let tx = tvd_v1();

    // Add + commit.
    let mut w = db.fjall_database.batch();
    add_transaction(&tx, false, &mut w, &db).expect("add_transaction failed");
    w.commit().expect("commit failed");

    // Remove + commit.
    let mut w = db.fjall_database.batch();
    remove_transaction(&tx.tx_hash, &mut w, &db).expect("remove_transaction failed");
    w.commit().expect("commit failed");

    // The tx must be gone.
    let snapshot = db.fjall_database.snapshot();
    let got = get_transaction_verification_data(&tx.tx_hash, &snapshot, &db);
    assert!(
        matches!(got, Err(TxPoolError::NotFound)),
        "removed tx should not be found, got {got:?}",
    );

    // Its key images must have been freed: re-adding the same tx now succeeds.
    let mut w = db.fjall_database.batch();
    add_transaction(&tx, false, &mut w, &db)
        .expect("re-adding after removal must succeed (key images freed)");
    w.commit().expect("commit failed");
}
