use crate::api::router;
use crate::state::tests::state_with_local_wallet;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
#[tokio::test]
async fn status_counts_accounts_without_reading_the_seed() {
    let (state, dir) = state_with_local_wallet();
    rusqlite::Connection::open(dir.path().join("server.db"))
        .unwrap()
        .execute("UPDATE metadata SET value='not-hex' WHERE key='seed'", [])
        .unwrap();

    let response = router(state)
        .oneshot(Request::get("/api/v1/status").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let status: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(status["account_count"], 5);
}

use super::*;
use crate::wallet::WALLET_BIRTHDAY_HEIGHT;
fn wallet_status(
    state: &'static str,
    fully_scanned_height: Option<u64>,
    error: Option<&str>,
) -> WalletSyncStatus {
    WalletSyncStatus {
        state,
        fully_scanned_height,
        fully_scanned_hash: fully_scanned_height.map(|_| "canonical".to_owned()),
        observed_hash: fully_scanned_height.map(|_| "canonical".to_owned()),
        observed_height: fully_scanned_height,
        last_success_at: Some(123),
        error: error.map(str::to_owned),
    }
}

fn settlement_for_test(
    node: Option<u64>,
    lightwalletd: Option<u64>,
    wallet: &WalletSyncStatus,
) -> SettlementStatus {
    let checkpoint = |height| ChainCheckpoint {
        height,
        hash: "canonical".to_owned(),
    };
    settlement_status(
        node.map(checkpoint).as_ref(),
        lightwalletd.map(checkpoint).as_ref(),
        wallet,
    )
}

#[test]
fn settlement_requires_all_three_components_at_the_same_tip() {
    let status = settlement_for_test(
        Some(105),
        Some(105),
        &wallet_status("ready", Some(105), None),
    );
    assert!(status.settled);
    assert_eq!(status.settlement_blocker, None);

    let status = settlement_for_test(
        Some(105),
        Some(104),
        &wallet_status("ready", Some(105), None),
    );
    assert!(!status.settled);
    assert_eq!(status.settlement_blocker, Some("height_mismatch"));

    let status = settlement_for_test(
        Some(105),
        Some(105),
        &wallet_status("ready", Some(106), None),
    );
    assert!(
        !status.settled,
        "a wallet ahead of the current tip needs reconciliation"
    );

    let canonical = ChainCheckpoint {
        height: 105,
        hash: "canonical".to_owned(),
    };
    let replacement = ChainCheckpoint {
        height: 105,
        hash: "replacement".to_owned(),
    };
    let wallet = wallet_status("ready", Some(105), None);
    let status = settlement_status(Some(&canonical), Some(&replacement), &wallet);
    assert_eq!(status.settlement_blocker, Some("chain_mismatch"));

    let status = settlement_status(Some(&replacement), Some(&replacement), &wallet);
    assert!(
        !status.settled,
        "a same-height reorg needs wallet reconciliation"
    );
    assert_eq!(status.settlement_blocker, Some("wallet_behind"));
}

#[test]
fn settlement_reports_the_first_blocking_component() {
    let cases = [
        (
            None,
            Some(105),
            wallet_status("ready", Some(105), None),
            "node_unavailable",
        ),
        (
            Some(105),
            None,
            wallet_status("ready", Some(105), None),
            "lightwalletd_unavailable",
        ),
        (
            Some(105),
            Some(105),
            wallet_status("error", Some(104), Some("scan failed")),
            "wallet_error",
        ),
        (
            Some(105),
            Some(105),
            wallet_status("syncing", Some(104), None),
            "wallet_syncing",
        ),
        (
            Some(105),
            Some(105),
            wallet_status("ready", None, None),
            "wallet_behind",
        ),
        (
            Some(105),
            Some(105),
            wallet_status("ready", Some(104), None),
            "wallet_behind",
        ),
    ];

    for (node, lightwalletd, wallet, expected) in cases {
        let status = settlement_for_test(node, lightwalletd, &wallet);
        assert!(!status.settled);
        assert_eq!(status.settlement_blocker, Some(expected));
    }
}

#[test]
fn settlement_allows_a_canonical_tip_before_the_wallet_birthday() {
    let tip = ChainCheckpoint {
        height: u64::from(WALLET_BIRTHDAY_HEIGHT) - 1,
        hash: "canonical".to_owned(),
    };
    let mut wallet = wallet_status("ready", None, None);
    wallet.observed_height = Some(tip.height);
    wallet.observed_hash = Some(tip.hash.clone());
    assert!(settlement_status(Some(&tip), Some(&tip), &wallet).settled);
    wallet.observed_hash = Some("orphaned".to_owned());
    assert!(!settlement_status(Some(&tip), Some(&tip), &wallet).settled);
}
