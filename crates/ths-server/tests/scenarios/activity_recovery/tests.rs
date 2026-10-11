use std::{future::pending, sync::Arc};

use anyhow::Result;
use tokio::sync::{Mutex, oneshot};

use super::{Activity, assert_persisted_send_response, preserve_scenario_failure};

fn activity(created_at: &str) -> Activity {
    Activity {
        id: "9b6a7ddd-8723-4163-b99e-7b16d62efef5".to_owned(),
        kind: "send".to_owned(),
        from_account: Some(1),
        to_account: 2,
        source_pool: "ironwood".to_owned(),
        destination_pool: "ironwood".to_owned(),
        amount_zatoshi: 1_000_000,
        txid: "b".repeat(64),
        block_hash: None,
        status: "broadcast".to_owned(),
        created_at: created_at.to_owned(),
    }
}

#[test]
fn timestamp_normalization_excludes_only_the_immediate_send_timestamp() -> Result<()> {
    let immediate = activity("");
    let persisted = activity("2026-09-23 12:00:00");
    assert_persisted_send_response(&immediate, &persisted)?;

    let mut changed_amount = persisted.clone();
    changed_amount.amount_zatoshi += 1;
    assert!(assert_persisted_send_response(&immediate, &changed_amount).is_err());

    let mut confirmed = persisted.clone();
    confirmed.status = "confirmed".to_owned();
    confirmed.block_hash = Some("c".repeat(64));
    let mut expected = persisted.clone();
    expected.status = "confirmed".to_owned();
    expected.block_hash = Some("c".repeat(64));
    assert_eq!(confirmed, expected);

    confirmed.created_at = "2026-09-23 12:00:01".to_owned();
    assert_ne!(confirmed, expected);
    Ok(())
}

#[test]
fn cleanup_error_does_not_mask_a_scenario_error() {
    let result = preserve_scenario_failure::<()>(
        Err(anyhow::anyhow!("scenario failure marker")),
        Err(anyhow::anyhow!("cleanup failure marker")),
    );
    let error = result.expect_err("scenario failure must remain the result");
    assert!(error.to_string().contains("scenario failure marker"));
    assert!(!error.to_string().contains("cleanup failure marker"));
}

#[tokio::test]
async fn test_local_cancellation_aborts_awaits_then_records_cleanup() -> Result<()> {
    let (cancel_sender, mut cancel_receiver) = oneshot::channel();
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut scenario = tokio::spawn(async {
        pending::<()>().await;
        Ok::<(), anyhow::Error>(())
    });
    cancel_sender
        .send(())
        .map_err(|_| anyhow::anyhow!("test-local cancellation receiver was unavailable"))?;

    tokio::select! {
        _ = &mut cancel_receiver => {
            scenario.abort();
            assert!(scenario.await.is_err());
            events.lock().await.push("scenario-aborted-and-awaited");
        }
        _ = &mut scenario => panic!("test scenario completed before cancellation"),
    }
    events.lock().await.push("cleanup-recorded");
    assert_eq!(
        events.lock().await.as_slice(),
        ["scenario-aborted-and-awaited", "cleanup-recorded"]
    );
    Ok(())
}
