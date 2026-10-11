use super::*;
use crate::api::{error::ApiResult, router};
use crate::state::tests::{offline_state, state_with_local_wallet};
use crate::{
    db::{Activity, Store, TREASURY_ACCOUNT_ID, USER_ACCOUNT_COUNT},
    state::AppState,
    wallet::RealWallet,
};
use axum::body::Body;
use axum::response::IntoResponse;
use axum::{
    Json,
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use tower::ServiceExt;
use zcash_protocol::value::MAX_MONEY;
#[test]
fn address_faucet_resolves_only_published_user_addresses() {
    let (state, _dir) = state_with_local_wallet();
    for id in 1..=USER_ACCOUNT_COUNT {
        let account = state.0.store.account(id).unwrap();
        assert_eq!(
            internal_faucet_destination(&state.0.store, &account.transparent_address).unwrap(),
            Some((id, "transparent"))
        );
        assert_eq!(
            internal_faucet_destination(&state.0.store, &account.unified_address).unwrap(),
            Some((id, "ironwood"))
        );
        assert_eq!(
            internal_faucet_destination(
                &state.0.store,
                &format!("{} ", account.transparent_address)
            )
            .unwrap(),
            None
        );
    }
    let treasury = state.0.store.account(TREASURY_ACCOUNT_ID).unwrap();
    for address in [
        treasury.transparent_address,
        treasury.unified_address,
        "unmatched".into(),
    ] {
        assert_eq!(
            internal_faucet_destination(&state.0.store, &address).unwrap(),
            None
        );
    }
}

#[test]
fn address_faucet_resolver_propagates_store_errors() {
    let store = Store::open(":memory:").unwrap();
    assert!(internal_faucet_destination(&store, "unmatched").is_err());
}

fn address_faucet_activity(store: &Store) -> Activity {
    let row = store
        .claim_faucet(2, "transparent", 100_000_000, "adapter-test")
        .unwrap();
    let row = store
        .record_prepared(&row.id, "faucet-txid", b"prepared bytes", 140)
        .unwrap();
    store.mark_broadcast(&row.id, &row.txid).unwrap()
}

#[test]
fn address_faucet_response_preserves_pending_and_confirmed_status() {
    let (state, _dir) = state_with_local_wallet();
    let address = state.0.store.account(2).unwrap().transparent_address;
    let request = || FaucetAddressRequest {
        idempotency_key: "address-test-key".into(),
        address: address.clone(),
        amount_zatoshi: 100_000_000,
    };
    let row = address_faucet_activity(&state.0.store);
    let pending = faucet_address_response(request(), row.clone()).unwrap();
    assert_eq!(pending.status, "broadcast");
    assert_eq!(pending.block_hash, None);
    let row = state
        .0
        .store
        .confirm(&row.id, &row.txid, "inclusion-hash")
        .unwrap();
    let value =
        serde_json::to_value(faucet_address_response(request(), row.clone()).unwrap()).unwrap();
    assert_eq!(
        value,
        json!({"address": address, "amount_zatoshi": 100_000_000, "txid": "faucet-txid", "block_hash": "inclusion-hash", "status": "confirmed"})
    );
    for block_hash in [None, Some(String::new())] {
        let mut invalid = row.clone();
        invalid.block_hash = block_hash;
        assert!(faucet_address_response(request(), invalid).is_err());
    }
}

struct RecordingAddressFaucetRuntime {
    internal_calls: std::sync::Mutex<Vec<(u8, String, u64, String)>>,
    external_calls: std::sync::Mutex<Vec<(String, u64)>>,
    activity: Activity,
    fail_internal: bool,
    fail_external: bool,
}

impl RecordingAddressFaucetRuntime {
    fn new(store: &Store) -> Self {
        let row = address_faucet_activity(store);
        Self {
            internal_calls: Default::default(),
            external_calls: Default::default(),
            activity: store.confirm(&row.id, &row.txid, "inclusion-hash").unwrap(),
            fail_internal: false,
            fail_external: false,
        }
    }
}

#[async_trait::async_trait]
impl AddressFaucetRuntime for RecordingAddressFaucetRuntime {
    async fn fund_internal(
        &self,
        id: u8,
        pool: &str,
        amount: u64,
        key: &str,
    ) -> anyhow::Result<Activity> {
        self.internal_calls
            .lock()
            .unwrap()
            .push((id, pool.into(), amount, key.into()));
        anyhow::ensure!(!self.fail_internal, "internal funding failed");
        Ok(self.activity.clone())
    }

    async fn fund_external(
        &self,
        req: FaucetAddressRequest,
    ) -> anyhow::Result<FaucetAddressResponse> {
        self.external_calls
            .lock()
            .unwrap()
            .push((req.address.clone(), req.amount_zatoshi));
        anyhow::ensure!(!self.fail_external, "external funding failed");
        Ok(FaucetAddressResponse {
            address: req.address,
            amount_zatoshi: req.amount_zatoshi,
            txid: "external-txid".into(),
            block_hash: Some("external-block".into()),
            status: "confirmed".into(),
        })
    }
}

#[tokio::test]
async fn address_faucet_dispatches_internal_transparent_and_unified() {
    let (state, _dir) = state_with_local_wallet();
    let runtime = RecordingAddressFaucetRuntime::new(&state.0.store);
    let account = state.0.store.account(2).unwrap();
    for (address, pool) in [
        (account.transparent_address, "transparent"),
        (account.unified_address, "ironwood"),
    ] {
        let Json(response) = execute_address_faucet(
            &state.0.store,
            &runtime,
            FaucetAddressRequest {
                idempotency_key: "address-test-key".into(),
                address: address.clone(),
                amount_zatoshi: 100_000_000,
            },
        )
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(
            serde_json::to_value(response).unwrap(),
            json!({"address": address, "amount_zatoshi": 100_000_000, "txid": "faucet-txid", "block_hash": "inclusion-hash", "status": "confirmed"})
        );
        let calls = runtime.internal_calls.lock().unwrap();
        let call = calls.last().unwrap();
        assert_eq!((call.0, call.1.as_str(), call.2), (2, pool, 100_000_000));
    }
    assert!(runtime.external_calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn address_faucet_retries_preserve_the_client_key() {
    let (state, _dir) = state_with_local_wallet();
    let runtime = RecordingAddressFaucetRuntime::new(&state.0.store);
    let address = state.0.store.account(2).unwrap().transparent_address;
    for _ in 0..2 {
        let Json(_response) = execute_address_faucet(
            &state.0.store,
            &runtime,
            FaucetAddressRequest {
                idempotency_key: "address-test-key".into(),
                address: address.clone(),
                amount_zatoshi: 100_000_000,
            },
        )
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
    }
    let calls = runtime.internal_calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].3, "address-test-key");
    assert_eq!(calls[1].3, "address-test-key");
    assert!(calls.iter().all(|call| require_key(&call.3).is_ok()));
    assert!(runtime.external_calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn address_faucet_validation_precedes_dispatch() {
    let (state, _dir) = state_with_local_wallet();
    let runtime = RecordingAddressFaucetRuntime::new(&state.0.store);
    let address = state.0.store.account(2).unwrap().transparent_address;
    for (address, amount) in [
        ("not-an-address".into(), 1),
        (address.clone(), 0),
        (address.clone(), 500_000_001),
    ] {
        let error = execute_address_faucet(
            &state.0.store,
            &runtime,
            FaucetAddressRequest {
                idempotency_key: "address-test-key".into(),
                address,
                amount_zatoshi: amount,
            },
        )
        .await
        .err()
        .unwrap();
        assert_eq!(error.into_response().status(), StatusCode::BAD_REQUEST);
    }
    assert!(runtime.internal_calls.lock().unwrap().is_empty());
    assert!(runtime.external_calls.lock().unwrap().is_empty());
    let Json(_response) = execute_address_faucet(
        &state.0.store,
        &runtime,
        FaucetAddressRequest {
            idempotency_key: "address-test-key".into(),
            address,
            amount_zatoshi: 500_000_000,
        },
    )
    .await
    .unwrap_or_else(|error| panic!("{}", error.message));
    assert_eq!(runtime.internal_calls.lock().unwrap()[0].2, 500_000_000);
}

#[tokio::test]
async fn address_faucet_dispatch_preserves_external_behavior() {
    let (state, _dir) = state_with_local_wallet();
    let mut runtime = RecordingAddressFaucetRuntime::new(&state.0.store);
    let treasury = state.0.store.account(TREASURY_ACCOUNT_ID).unwrap();
    for address in [
        treasury.transparent_address.clone(),
        treasury.unified_address,
    ] {
        let Json(response) = execute_address_faucet(
            &state.0.store,
            &runtime,
            FaucetAddressRequest {
                idempotency_key: "address-test-key".into(),
                address: address.clone(),
                amount_zatoshi: 100_000_000,
            },
        )
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(
            serde_json::to_value(response).unwrap(),
            json!({"address": address, "amount_zatoshi": 100_000_000, "txid": "external-txid", "block_hash": "external-block", "status": "confirmed"})
        );
        assert_eq!(
            runtime.external_calls.lock().unwrap().last().unwrap(),
            &(address, 100_000_000)
        );
    }
    runtime.fail_external = true;
    assert!(
        execute_address_faucet(
            &state.0.store,
            &runtime,
            FaucetAddressRequest {
                idempotency_key: "address-test-key".into(),
                address: treasury.transparent_address,
                amount_zatoshi: 1
            }
        )
        .await
        .is_err()
    );
    assert!(runtime.internal_calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn address_faucet_internal_errors_do_not_fall_back() {
    let (state, _dir) = state_with_local_wallet();
    let mut runtime = RecordingAddressFaucetRuntime::new(&state.0.store);
    let address = state.0.store.account(2).unwrap().transparent_address;
    runtime.fail_internal = true;
    assert!(
        execute_address_faucet(
            &state.0.store,
            &runtime,
            FaucetAddressRequest {
                idempotency_key: "address-test-key".into(),
                address: address.clone(),
                amount_zatoshi: 100_000_000
            }
        )
        .await
        .is_err()
    );
    runtime.fail_internal = false;
    runtime.activity.status = "broadcast".into();
    runtime.activity.block_hash = None;
    let Json(pending) = execute_address_faucet(
        &state.0.store,
        &runtime,
        FaucetAddressRequest {
            idempotency_key: "address-test-key".into(),
            address: address.clone(),
            amount_zatoshi: 100_000_000,
        },
    )
    .await
    .unwrap_or_else(|error| panic!("{}", error.message));
    assert_eq!(pending.status, "broadcast");
    assert_eq!(pending.block_hash, None);
    assert_eq!(runtime.internal_calls.lock().unwrap().len(), 2);
    let broken_store = Store::open(":memory:").unwrap();
    assert!(
        execute_address_faucet(
            &broken_store,
            &runtime,
            FaucetAddressRequest {
                idempotency_key: "address-test-key".into(),
                address,
                amount_zatoshi: 1
            }
        )
        .await
        .is_err()
    );
    assert_eq!(runtime.internal_calls.lock().unwrap().len(), 2);
    assert!(runtime.external_calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn send_rejects_invalid_pools_before_wallet_work() {
    let (state, _dir) = state_with_local_wallet();
    for (source_pool, destination_pool) in [("sapling", "ironwood"), ("ironwood", "sapling")] {
        let response = router(state.clone())
            .oneshot(
                Request::post("/api/v1/send")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&json!({
                            "from_account": 1,
                            "to_account": 2,
                            "source_pool": source_pool,
                            "destination_pool": destination_pool,
                            "amount_zatoshi": 1,
                            "idempotency_key": "invalid-pool",
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}

#[test]
fn reserves_the_treasury_account_from_public_operations() {
    for id in 1..=USER_ACCOUNT_COUNT {
        assert!(require_user_account(id).is_ok());
    }
    assert!(require_user_account(TREASURY_ACCOUNT_ID).is_err());
}

fn is_bad_request<T>(result: ApiResult<T>) -> bool {
    matches!(result, Err(error) if error.status == StatusCode::BAD_REQUEST)
}

#[test]
fn rejects_orchard_as_a_pool_after_nu6_3() {
    assert!(require_pool("ironwood", "pool").is_ok());
    assert!(require_pool("transparent", "pool").is_ok());
    let Err(error) = require_pool("orchard", "destination_pool") else {
        panic!("orchard must be rejected");
    };
    assert_eq!(error.status, StatusCode::BAD_REQUEST);
    assert!(error.message.contains("use ironwood"));
    assert!(is_bad_request(require_pool("sapling", "pool")));
}

#[test]
fn absent_and_null_memos_differ_from_an_explicit_empty_memo() {
    let request = |memo: Option<Value>| {
        let mut body = json!({
            "from_account": 1, "to_account": 2, "source_pool": "ironwood",
            "destination_pool": "ironwood", "amount_zatoshi": 1,
            "idempotency_key": "memo-test-key",
        });
        if let Some(memo) = memo {
            body["memo"] = memo;
        }
        serde_json::from_value::<SendRequest>(body).expect("valid request shape")
    };
    assert_eq!(request(None).memo, None);
    assert_eq!(request(Some(Value::Null)).memo, None);
    assert_eq!(request(Some(json!(""))).memo.as_deref(), Some(""));

    assert!(matches!(parse_memo(None, "ironwood"), Ok(None)));
    assert!(matches!(parse_memo(None, "transparent"), Ok(None)));
    let Ok(Some(empty)) = parse_memo(Some(""), "ironwood") else {
        panic!("an explicit empty memo must be kept");
    };
    assert_eq!(empty.as_array(), &[0u8; 512]);
}

#[test]
fn any_present_memo_is_rejected_for_a_transparent_destination() {
    assert!(is_bad_request(parse_memo(Some("hi"), "transparent")));
    assert!(is_bad_request(parse_memo(Some(""), "transparent")));
}

#[test]
fn memos_are_bounded_by_utf8_bytes() {
    let Ok(Some(memo)) = parse_memo(Some("thanks for lunch"), "ironwood") else {
        panic!("expected an encoded memo");
    };
    assert_eq!(&memo.as_slice()[..16], b"thanks for lunch");
    assert!(memo.as_slice()[16..].iter().all(|byte| *byte == 0));

    assert!(parse_memo(Some(&"a".repeat(512)), "ironwood").is_ok());
    assert!(is_bad_request(parse_memo(
        Some(&"a".repeat(513)),
        "ironwood"
    )));

    // Three bytes per character: 170 fit (510 bytes), 171 do not (513).
    let Ok(Some(cjk)) = parse_memo(Some(&"桜".repeat(170)), "ironwood") else {
        panic!("510 bytes of multibyte text must fit");
    };
    assert_eq!(&cjk.as_slice()[..510], "桜".repeat(170).as_bytes());
    assert!(is_bad_request(parse_memo(
        Some(&"桜".repeat(171)),
        "ironwood"
    )));

    // Four bytes per character: exactly 512 fits, one more does not.
    assert!(parse_memo(Some(&"🌸".repeat(128)), "ironwood").is_ok());
    assert!(is_bad_request(parse_memo(
        Some(&"🌸".repeat(129)),
        "ironwood"
    )));
}

#[test]
fn memos_must_not_end_with_nul() {
    assert!(is_bad_request(parse_memo(Some("hi\0"), "ironwood")));
    assert!(is_bad_request(parse_memo(Some("\0"), "ironwood")));
    assert!(parse_memo(Some("a\0b"), "ironwood").is_ok());
}

const REPLAY_KEY: &str = "existing-idempotency-key";

/// A state with a real store and offline wallet whose node RPC refuses
/// connections, so any synchronization or network access surfaces as a
/// 500 rather than the 400 or replay these tests expect.
fn valid_send() -> Value {
    json!({
        "from_account": 1,
        "to_account": 2,
        "source_pool": "ironwood",
        "destination_pool": "ironwood",
        "amount_zatoshi": 100_000,
        "idempotency_key": REPLAY_KEY,
    })
}

async fn post_send(state: &AppState, body: &Value) -> (StatusCode, Value) {
    let response = router(state.clone())
        .oneshot(
            Request::post("/api/v1/send")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn activity_ids(state: &AppState) -> Vec<String> {
    state
        .0
        .store
        .activities(100)
        .unwrap()
        .into_iter()
        .map(|activity| activity.id)
        .collect()
}

#[test]
fn send_validation_allows_same_account_only_between_pools() {
    for to in [1, 2] {
        for source in ["ironwood", "transparent"] {
            for destination in ["ironwood", "transparent"] {
                let mut body = valid_send();
                body["to_account"] = json!(to);
                body["source_pool"] = json!(source);
                body["destination_pool"] = json!(destination);
                let req = serde_json::from_value::<SendRequest>(body).unwrap();
                let result = validate_send(&req);
                if to == 1 && source == destination {
                    let error = result.unwrap_err();
                    assert_eq!(error.status, StatusCode::BAD_REQUEST);
                    assert_eq!(
                        error.message,
                        "choose a different account or a different destination pool"
                    );
                } else {
                    assert!(
                        matches!(result, Ok(None)),
                        "{to}: {source} -> {destination}"
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn invalid_sends_are_rejected_before_replay_or_synchronization() {
    let (state, _dir) = offline_state();
    let original = state
        .0
        .store
        .claim_transfer(1, 2, "ironwood", "ironwood", 100_000, REPLAY_KEY, None)
        .unwrap();
    let before = activity_ids(&state);

    let with = |fields: &[(&str, Value)]| {
        let mut body = valid_send();
        for (field, value) in fields {
            body[*field] = value.clone();
        }
        body
    };
    let transparent = ("destination_pool", json!("transparent"));
    let cases = [
        ("short key", with(&[("idempotency_key", json!("short"))])),
        (
            "key with spaces",
            with(&[("idempotency_key", json!("has spaces here"))]),
        ),
        ("account 0", with(&[("from_account", json!(0))])),
        (
            "treasury account",
            with(&[("to_account", json!(TREASURY_ACCOUNT_ID))]),
        ),
        ("same account and pool", with(&[("to_account", json!(1))])),
        (
            "same account and transparent pool",
            with(&[
                ("to_account", json!(1)),
                ("source_pool", json!("transparent")),
                ("destination_pool", json!("transparent")),
            ]),
        ),
        (
            "same account and unsupported source",
            with(&[("to_account", json!(1)), ("source_pool", json!("sapling"))]),
        ),
        (
            "same account and unsupported destination",
            with(&[
                ("to_account", json!(1)),
                ("destination_pool", json!("orchard")),
            ]),
        ),
        (
            "same account unshield with memo",
            with(&[
                ("to_account", json!(1)),
                ("destination_pool", json!("transparent")),
                ("memo", json!("hi")),
            ]),
        ),
        (
            "bad source pool",
            with(&[("source_pool", json!("sapling"))]),
        ),
        (
            "bad destination pool",
            with(&[("destination_pool", json!("sprout"))]),
        ),
        ("zero amount", with(&[("amount_zatoshi", json!(0))])),
        (
            "amount over MAX_MONEY",
            with(&[("amount_zatoshi", json!(MAX_MONEY + 1))]),
        ),
        ("memo too long", with(&[("memo", json!("a".repeat(513)))])),
        ("memo ending in NUL", with(&[("memo", json!("hi\u{0}"))])),
        (
            "memo to transparent",
            with(&[transparent.clone(), ("memo", json!("hi"))]),
        ),
        (
            "empty memo to transparent",
            with(&[transparent, ("memo", json!(""))]),
        ),
    ];
    for (name, body) in cases {
        let (status, response) = post_send(&state, &body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{name}: {response}");
        assert_eq!(activity_ids(&state), before, "{name} changed activity");
    }
    assert_eq!(
        state
            .0
            .store
            .activity_for_key(REPLAY_KEY)
            .unwrap()
            .unwrap()
            .id,
        original.id
    );
}

#[tokio::test]
async fn same_account_cross_pool_replays_preserve_activity_and_identity() {
    for (source, destination) in [("ironwood", "transparent"), ("transparent", "ironwood")] {
        let (state, _dir) = offline_state();
        let original = state
            .0
            .store
            .claim_transfer(1, 1, source, destination, 100_000, REPLAY_KEY, None)
            .unwrap();
        state
            .0
            .store
            .record_prepared(&original.id, "txid", b"signed transaction", 0)
            .unwrap();
        state.0.store.mark_broadcast(&original.id, "txid").unwrap();
        state
            .0
            .store
            .confirm(&original.id, "txid", &"c".repeat(64))
            .unwrap();
        let before = activity_ids(&state);
        let mut body = valid_send();
        body["to_account"] = json!(1);
        body["source_pool"] = json!(source);
        body["destination_pool"] = json!(destination);
        for _ in 0..2 {
            let (status, response) = post_send(&state, &body).await;
            assert_eq!(status, StatusCode::OK, "{response}");
            assert_eq!(response["id"], original.id);
            assert_eq!(response["txid"], "txid");
            assert_eq!(response["from_account"], 1);
            assert_eq!(response["to_account"], 1);
            assert_eq!(response["source_pool"], source);
            assert_eq!(response["destination_pool"], destination);
        }
        for changed in [("amount_zatoshi", json!(100_001)), ("to_account", json!(2))] {
            let mut conflict = body.clone();
            conflict[changed.0] = changed.1;
            assert_eq!(post_send(&state, &conflict).await.0, StatusCode::CONFLICT);
        }
        let mut reverse = body.clone();
        reverse["source_pool"] = json!(destination);
        reverse["destination_pool"] = json!(source);
        assert_eq!(post_send(&state, &reverse).await.0, StatusCode::CONFLICT);
        assert_eq!(activity_ids(&state), before);
    }
}

#[tokio::test]
async fn a_valid_replay_returns_the_original_without_the_wallet_or_network() {
    let (state, _dir) = offline_state();
    let original = state
        .0
        .store
        .claim_transfer(1, 2, "ironwood", "ironwood", 100_000, REPLAY_KEY, None)
        .unwrap();
    state
        .0
        .store
        .record_prepared(&original.id, "txid", b"signed transaction", 0)
        .unwrap();
    state.0.store.mark_broadcast(&original.id, "txid").unwrap();
    state
        .0
        .store
        .confirm(&original.id, "txid", &"c".repeat(64))
        .unwrap();
    let before = activity_ids(&state);

    for memo in [None, Some(Value::Null)] {
        let mut body = valid_send();
        if let Some(memo) = memo {
            body["memo"] = memo;
        }
        let (status, response) = post_send(&state, &body).await;
        assert_eq!(status, StatusCode::OK, "{response}");
        assert_eq!(response["id"], original.id);
        assert_eq!(response["txid"], "txid");
    }
    for memo in ["", "different memo"] {
        let mut body = valid_send();
        body["memo"] = json!(memo);
        let (status, _) = post_send(&state, &body).await;
        assert_eq!(status, StatusCode::CONFLICT);
    }
    assert_eq!(activity_ids(&state), before);

    // Guard for the harness itself: a send that is not a replay must reach
    // the (unreachable) network and fail, so the 200s above prove none did.
    let mut fresh = valid_send();
    fresh["idempotency_key"] = json!("a-key-never-used-before");
    let (status, _) = post_send(&state, &fresh).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(activity_ids(&state), before);
}

#[test]
fn faucet_accepts_only_regtest_unified_and_transparent_addresses() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let account = store.account(1).unwrap();

    assert!(require_faucet_address(&account.unified_address).is_ok());
    assert!(require_faucet_address(&account.transparent_address).is_ok());
    assert!(require_faucet_address("not-an-address").is_err());
}

#[tokio::test]
async fn address_faucet_rejects_missing_or_invalid_operation_keys_before_network_work() {
    let (state, _dir) = state_with_local_wallet();
    let address = state.0.store.account(1).unwrap().unified_address;
    for key in [None, Some("short"), Some("has spaces")] {
        let mut body = json!({"address": address, "amount_zatoshi": 1});
        if let Some(key) = key {
            body["idempotency_key"] = json!(key);
        }
        let response = router(state.clone())
            .oneshot(
                Request::post("/api/v1/faucet/address")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}

#[tokio::test]
async fn internal_address_faucet_replays_preserve_activity_after_store_reopen() {
    for pool in ["transparent", "ironwood"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.db");
        let store = Store::open(&path).unwrap();
        store.initialize().unwrap();
        let account = store.account(2).unwrap();
        let address = if pool == "transparent" {
            account.transparent_address
        } else {
            account.unified_address
        };
        let original = store.claim_faucet(2, pool, 123, "internal-replay").unwrap();
        store
            .record_prepared(&original.id, "original-txid", b"signed bytes", 140)
            .unwrap();
        store
            .confirm(&original.id, "original-txid", "original-block")
            .unwrap();
        drop(store);

        let store = Store::open(&path).unwrap();
        store.initialize().unwrap();
        let wallet = RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();
        let state = AppState::new(store, wallet, "http://127.0.0.1:1".into(), "test".into());
        let body = json!({"address": address, "amount_zatoshi": 123, "idempotency_key": "internal-replay"});
        for _ in 0..2 {
            let response = router(state.clone())
                .oneshot(
                    Request::post("/api/v1/faucet/address")
                        .header("content-type", "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let result: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(result["txid"], "original-txid");
            assert_eq!(result["block_hash"], "original-block");
            assert_eq!(result["status"], "confirmed");
        }
        for (field, value) in [
            ("amount_zatoshi", json!(124)),
            (
                "address",
                json!(state.0.store.account(3).unwrap().transparent_address),
            ),
        ] {
            let mut conflict = body.clone();
            conflict[field] = value;
            let response = router(state.clone())
                .oneshot(
                    Request::post("/api/v1/faucet/address")
                        .header("content-type", "application/json")
                        .body(Body::from(conflict.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::CONFLICT);
        }
        let activities = state.0.store.activities(100).unwrap();
        assert_eq!(activities.len(), 1);
        assert_eq!(activities[0].id, original.id);
        assert_eq!(activities[0].txid, "original-txid");
    }
}

#[tokio::test]
async fn confirmed_address_faucet_replays_without_sending_a_second_payment() {
    let (state, _dir) = state_with_local_wallet();
    let address = state.0.store.account(1).unwrap().unified_address;
    let claim = state
        .0
        .store
        .claim_address_faucet(&address, 123, "replay-address")
        .unwrap();
    state
        .0
        .store
        .record_address_prepared(&claim.id, "original-txid", b"original bytes", 140)
        .unwrap();
    state
        .0
        .store
        .confirm_address(&claim.id, "original-txid", "original-block")
        .unwrap();
    let body = json!({"address":address,"amount_zatoshi":123,"idempotency_key":"replay-address"});

    for _ in 0..2 {
        let response = router(state.clone())
            .oneshot(
                Request::post("/api/v1/faucet/address")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let result: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(result["txid"], "original-txid");
        assert_eq!(result["block_hash"], "original-block");
        assert_eq!(result["status"], "confirmed");
    }
    let mut conflict = body;
    conflict["amount_zatoshi"] = json!(124);
    let response = router(state)
        .oneshot(
            Request::post("/api/v1/faucet/address")
                .header("content-type", "application/json")
                .body(Body::from(conflict.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
}

/// An unknown pool is a client error, not the wallet's internal `bail!`
/// surfacing as a 500 that the dashboard reports as an unexpected error.
#[tokio::test]
async fn send_quote_rejects_unknown_pools_as_bad_requests() {
    let (state, _dir) = state_with_local_wallet();
    let app = router(state);
    for (body, message) in [
        (
            json!({"from_account": 1, "source_pool": "sapling", "destination_pool": "ironwood"}),
            "source_pool must be transparent or ironwood",
        ),
        (
            json!({"from_account": 1, "source_pool": "ironwood", "destination_pool": "sapling"}),
            "destination_pool must be transparent or ironwood",
        ),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::post("/api/v1/send/quote")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(
            String::from_utf8_lossy(&bytes).contains(message),
            "expected {message} in {bytes:?}"
        );
    }
}
