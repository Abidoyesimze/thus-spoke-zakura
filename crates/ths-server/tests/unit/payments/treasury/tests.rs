use super::*;
use crate::{
    db::{Account, TREASURY_ACCOUNT_ID},
    wallet::{PaymentError, PreparedPayment},
};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
struct RecordingFaucetRuntime {
    events: Mutex<Vec<String>>,
    funds_available: AtomicBool,
    reward_discovered: AtomicBool,
    height: AtomicUsize,
    cursor: Mutex<Option<crate::db::TreasuryCursor>>,
}

impl Default for RecordingFaucetRuntime {
    fn default() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
            funds_available: AtomicBool::new(false),
            reward_discovered: AtomicBool::new(false),
            height: AtomicUsize::new(2),
            cursor: Mutex::new(None),
        }
    }
}

#[async_trait::async_trait]
impl FaucetRuntime for RecordingFaucetRuntime {
    async fn prepare_payment(
        &self,
        _activity_id: Option<&str>,
        _seed: &str,
        _destination: &str,
        _amount_zatoshi: u64,
    ) -> anyhow::Result<PreparedPayment> {
        self.events.lock().unwrap().push("prepare".into());
        if self.funds_available.load(Ordering::SeqCst) {
            Ok(PreparedPayment {
                txid: "recovered-txid".into(),
                raw_transaction: b"recovered transaction".to_vec(),
                expiry_height: 240,
            })
        } else {
            Err(anyhow::Error::new(PaymentError::InsufficientFunds {
                available: 0,
                required: 100_010_000,
            }))
        }
    }
    async fn chain_height(&self) -> anyhow::Result<u64> {
        Ok(self.height.load(Ordering::SeqCst) as u64)
    }
    async fn block_hash(&self, height: u32) -> anyhow::Result<String> {
        Ok(format!("{height:064x}"))
    }
    async fn synchronize_latest(&self) -> anyhow::Result<()> {
        self.events.lock().unwrap().push("sync".into());
        Ok(())
    }
    async fn treasury_cursor(&self) -> anyhow::Result<Option<crate::db::TreasuryCursor>> {
        Ok(self.cursor.lock().unwrap().clone())
    }
    async fn initialize_treasury_cursor(
        &self,
        receiver: &str,
        height: u32,
        hash: &str,
    ) -> anyhow::Result<crate::db::TreasuryCursor> {
        let cursor = crate::db::TreasuryCursor {
            receiver: receiver.into(),
            height,
            block_hash: hash.into(),
        };
        *self.cursor.lock().unwrap() = Some(cursor.clone());
        Ok(cursor)
    }
    async fn discover_reward(
        &self,
        cursor: &crate::db::TreasuryCursor,
        _treasury: &Account,
    ) -> anyhow::Result<crate::db::TreasuryCursor> {
        assert_eq!(cursor.height, 1);
        self.events.lock().unwrap().push("discover:2".into());
        self.reward_discovered.store(true, Ordering::SeqCst);
        let cursor = crate::db::TreasuryCursor {
            receiver: cursor.receiver.clone(),
            height: 2,
            block_hash: format!("{:064x}", 2),
        };
        *self.cursor.lock().unwrap() = Some(cursor.clone());
        Ok(cursor)
    }
    async fn shield_coinbase(
        &self,
        _seed: &str,
        _treasury: &Account,
        minimum_net: u64,
    ) -> anyhow::Result<()> {
        assert_eq!(minimum_net, 100_010_000);
        self.events.lock().unwrap().push("shield".into());
        if !self.reward_discovered.load(Ordering::SeqCst) {
            return Err(anyhow::Error::new(PaymentError::TreasuryExhausted));
        }
        self.funds_available.store(true, Ordering::SeqCst);
        Ok(())
    }
    async fn mine_and_sync(&self, blocks: u32) -> anyhow::Result<()> {
        self.events.lock().unwrap().push(format!("mine:{blocks}"));
        self.height.fetch_add(blocks as usize, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn insufficient_funds_discovers_only_the_needed_mature_reward_and_retries() {
    let runtime = RecordingFaucetRuntime::default();
    let treasury = Account {
        id: TREASURY_ACCOUNT_ID,
        name: "Account 6".into(),
        unified_address: "uregtest-treasury".into(),
        transparent_address: "tmTreasury".into(),
        unified_full_viewing_key: None,
        transparent_zatoshi: 0,
        ironwood_zatoshi: 0,
    };

    let prepared = prepare_with_replenishment(
        &runtime,
        None,
        "seed",
        &treasury,
        "uregtest-recipient",
        100_000_000,
    )
    .await
    .unwrap();
    assert_eq!(prepared.txid, "recovered-txid");
    assert_eq!(runtime.cursor.lock().unwrap().as_ref().unwrap().height, 2);
    assert_eq!(
        runtime.events.into_inner().unwrap(),
        [
            "prepare",
            "shield",
            "mine:99",
            "discover:2",
            "shield",
            "mine:1",
            "prepare"
        ]
    );
}
