use std::collections::HashSet;

use anyhow::{Context, Result, bail};
use rusqlite::{OptionalExtension, params};
use schemerz_rusqlite::RusqliteMigration;
use uuid::Uuid;
use zcash_client_backend::data_api::ll::LowLevelWalletRead;
use zcash_client_sqlite::{
    ExtensionTransaction,
    wallet::init::{WalletMigrationError, migrations},
};
use zcash_protocol::TxId;

use super::*;
use crate::db::TreasuryCursor;

// This persisted schema name and migration UUID predate the ths rename and
// must remain consistent with wallets that already applied the migration.
pub(super) struct TreasuryCursorMigration;

impl schemerz::Migration<Uuid> for TreasuryCursorMigration {
    fn id(&self) -> Uuid {
        Uuid::from_u128(0xd1b79365_76c1_4e9a_86a3_9c36d6a1b5e8)
    }

    fn dependencies(&self) -> HashSet<Uuid> {
        migrations::V_0_22_0_RC2.iter().copied().collect()
    }

    fn description(&self) -> &'static str {
        "Atomic treasury discovery cursor"
    }
}

impl RusqliteMigration for TreasuryCursorMigration {
    type Error = WalletMigrationError;

    fn up(&self, db: &rusqlite::Transaction<'_>) -> Result<(), Self::Error> {
        db.execute_batch(
            "CREATE TABLE ext_tsz_treasury_cursor (
                account_uuid TEXT PRIMARY KEY,
                receiver TEXT NOT NULL,
                height INTEGER NOT NULL CHECK(height >= 1),
                block_hash TEXT NOT NULL,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );",
        )?;
        Ok(())
    }
}

fn cursor(
    ext: &ExtensionTransaction<'_>,
    account_id: AccountUuid,
) -> Result<Option<TreasuryCursor>> {
    Ok(ext
        .query_row(
            "SELECT receiver,height,block_hash FROM ext_tsz_treasury_cursor WHERE account_uuid=?1",
            [account_id.expose_uuid().to_string()],
            |row| {
                Ok(TreasuryCursor {
                    receiver: row.get(0)?,
                    height: row.get(1)?,
                    block_hash: row.get(2)?,
                })
            },
        )
        .optional()?)
}

impl RealWallet {
    pub(crate) async fn treasury_cursor(&self) -> Result<Option<TreasuryCursor>> {
        let account_id = self.account_ids[usize::from(crate::db::TREASURY_ACCOUNT_ID - 1)];
        self.with_db(move |db| {
            db.transactionally_with_extension::<_, _, anyhow::Error>(|_, ext| {
                cursor(ext, account_id)
            })
        })
        .await
    }

    pub(crate) async fn initialize_treasury_cursor(
        &self,
        receiver: &str,
        height: u32,
        block_hash: &str,
    ) -> Result<TreasuryCursor> {
        if height != WALLET_BIRTHDAY_HEIGHT - 1 {
            bail!("treasury discovery must start immediately before the wallet birthday");
        }
        let receiver = receiver.to_owned();
        let block_hash = block_hash.to_owned();
        let account_id = self.account_ids[usize::from(crate::db::TREASURY_ACCOUNT_ID - 1)];
        self.with_db(move |db| {
            if !db
                .get_transparent_receivers(account_id, true, true)?
                .keys()
                .any(|address| address.encode(&regtest_network()) == receiver)
            {
                bail!("treasury cursor receiver does not belong to the treasury account");
            }
            db.transactionally_with_extension::<_, _, anyhow::Error>(|_, ext| {
                ext.execute("INSERT INTO ext_tsz_treasury_cursor(account_uuid,receiver,height,block_hash) VALUES(?1,?2,?3,?4) ON CONFLICT(account_uuid) DO UPDATE SET receiver=excluded.receiver,height=excluded.height,block_hash=excluded.block_hash,updated_at=CURRENT_TIMESTAMP WHERE receiver != excluded.receiver",
                    params![account_id.expose_uuid().to_string(),receiver,height,block_hash])?;
                cursor(ext, account_id)?.context("treasury cursor was not initialized")
            })
        }).await
    }

    pub(crate) async fn commit_treasury_discovery(
        &self,
        expected: &TreasuryCursor,
        checkpoint: &ChainCheckpoint,
        raw: Option<&str>,
        required_spenders: &[(u32, String)],
    ) -> Result<TreasuryCursor> {
        if checkpoint.height != u64::from(expected.height) + 1 {
            bail!("treasury cursor must advance exactly one block");
        }
        let height = u32::try_from(checkpoint.height)?;
        let expected = expected.clone();
        let checkpoint = checkpoint.clone();
        let raw = raw.map(str::to_owned);
        let required_spenders = required_spenders.to_vec();
        let account_id = self.account_ids[usize::from(crate::db::TREASURY_ACCOUNT_ID - 1)];
        self.with_db(move |db| {
            db.transactionally_with_extension::<_, _, anyhow::Error>(|wallet, ext| {
                if let Some(raw) = raw {
                    let bytes = hex::decode(raw).context("invalid treasury transaction hex")?;
                    let tx = Transaction::read(&bytes[..], BranchId::for_height(&regtest_network(), height.into()))?;
                    let bundle = tx.transparent_bundle().filter(|bundle| bundle.is_coinbase()).context("treasury discovery requires a coinbase transaction")?;
                    let treasury_indices = bundle.vout.iter().enumerate().filter_map(|(index, output)| {
                        output.recipient_address().filter(|address| address.encode(&regtest_network()) == expected.receiver).map(|_| index as u32)
                    }).collect::<Vec<_>>();
                    if treasury_indices.is_empty() {
                        bail!("coinbase does not pay the configured treasury receiver");
                    }
                    decrypt_and_store_transaction(&regtest_network(), wallet, &tx, Some(height.into()))?;
                    if wallet.get_transaction(tx.txid())?.is_none() {
                        bail!("treasury reward was not stored by the wallet");
                    }
                    for index in &treasury_indices {
                        let output = wallet.get_wallet_transparent_output(&OutPoint::new(*tx.txid().as_ref(), *index), None)?.context("treasury output was not stored by the wallet")?;
                        if output.recipient_account() != Some(&account_id) {
                            bail!("treasury output belongs to a different wallet account");
                        }
                    }
                    for (index, spender) in required_spenders {
                        if !treasury_indices.contains(&index) { bail!("spender evidence does not refer to a treasury output"); }
                        let spender = wallet.get_transaction(TxId::from_hex(&spender).context("invalid spender transaction id")?)?.context("treasury output spender is not stored in the wallet")?;
                        let outpoint = OutPoint::new(*tx.txid().as_ref(), index);
                        if !spender.transparent_bundle().is_some_and(|bundle| bundle.vin.iter().any(|input| input.prevout() == &outpoint)) {
                            bail!("stored transaction does not spend the treasury output");
                        }
                    }
                } else if !required_spenders.is_empty() {
                    bail!("treasury spender verification requires its coinbase transaction");
                }
                let changed = ext.execute("UPDATE ext_tsz_treasury_cursor SET height=?1,block_hash=?2,updated_at=CURRENT_TIMESTAMP WHERE account_uuid=?3 AND receiver=?4 AND height=?5 AND block_hash=?6",
                    params![height,checkpoint.hash,account_id.expose_uuid().to_string(),expected.receiver,expected.height,expected.block_hash])?;
                if changed != 1 { bail!("treasury cursor changed during discovery"); }
                cursor(ext, account_id)?.context("treasury cursor disappeared")
            })
        }).await
    }

    pub(crate) async fn known_spending_transactions(
        &self,
        txid: &str,
        index: u32,
    ) -> Result<Vec<String>> {
        let txid = TxId::from_hex(txid).context("invalid transaction id")?;
        self.with_db(move |db| {
            db.transactionally_with_extension::<_, _, anyhow::Error>(|wallet, ext| {
                let outpoint = OutPoint::new(*txid.as_ref(), index);
                let mut spenders = Vec::new();
                let mut previous_row = 0i64;
                loop {
                    // The SDK owns spentness, including shielding created before this extension.
                    // Review this extension read when upgrading the pinned SDK schema.
                    let candidate = ext.query_row(
                        "SELECT spender.id_tx,spender.txid FROM transactions origin
                         JOIN transparent_received_outputs output ON output.transaction_id=origin.id_tx
                         JOIN transparent_received_output_spends spend ON spend.transparent_received_output_id=output.id
                         JOIN transactions spender ON spender.id_tx=spend.transaction_id
                         WHERE origin.txid=?1 AND output.output_index=?2 AND spender.id_tx>?3
                         ORDER BY spender.id_tx LIMIT 1",
                        params![txid.as_ref(),index,previous_row],
                        |row| Ok((row.get::<_,i64>(0)?,row.get::<_,Vec<u8>>(1)?)),
                    ).optional()?;
                    let Some((rowid, candidate)) = candidate else { break; };
                    previous_row = rowid;
                    let candidate = TxId::from_bytes(candidate.try_into().map_err(|_| anyhow::anyhow!("invalid wallet spender transaction id"))?);
                    if let Some(tx) = wallet.get_transaction(candidate)?
                        && tx.transparent_bundle().is_some_and(|bundle| bundle.vin.iter().any(|input| input.prevout() == &outpoint)) {
                        spenders.push(candidate.to_string());
                    }
                }
                Ok(spenders)
            })
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use transparent::bundle::{Authorized as TransparentAuthorized, Bundle, TxIn};
    use zcash_primitives::transaction::{Authorized, TransactionData, TxVersion};
    use zcash_script::script::Evaluable;

    use super::*;
    use crate::db::{Store, TREASURY_ACCOUNT_ID};

    fn setup() -> (tempfile::TempDir, Store, RealWallet) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("server.db")).unwrap();
        store.initialize().unwrap();
        let wallet = RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();
        (dir, store, wallet)
    }

    fn transaction(receiver: &str, prevout: OutPoint, tag: u32) -> Transaction {
        let Address::Transparent(address) = Address::decode(&regtest_network(), receiver).unwrap()
        else {
            panic!("transparent receiver required")
        };
        TransactionData::<Authorized>::from_parts(
            TxVersion::V5,
            BranchId::for_height(&regtest_network(), 2.into()),
            tag,
            200.into(),
            Some(Bundle {
                vin: vec![TxIn::from_parts(
                    prevout,
                    Script(script::Code(vec![1, 2])),
                    u32::MAX,
                )],
                vout: vec![TxOut::new(
                    Zatoshis::from_u64(100_000_000).unwrap(),
                    Script(script::Code(address.script().to_bytes())),
                )],
                authorization: TransparentAuthorized,
            }),
            None,
            None,
            None,
        )
        .freeze()
        .unwrap()
    }

    fn raw(tx: &Transaction) -> String {
        let mut bytes = Vec::new();
        tx.write(&mut bytes).unwrap();
        hex::encode(bytes)
    }

    fn checkpoint(height: u64, byte: u8) -> ChainCheckpoint {
        ChainCheckpoint {
            height,
            hash: hex::encode([byte; 32]),
        }
    }

    #[tokio::test]
    async fn cursor_at_birthday_floor_can_rewind_before_the_first_scan() {
        let (_dir, store, wallet) = setup();
        let receiver = store
            .account(TREASURY_ACCOUNT_ID)
            .unwrap()
            .transparent_address;
        wallet
            .initialize_treasury_cursor(&receiver, 1, &"01".repeat(32))
            .await
            .unwrap();
        assert!(wallet.scanned_checkpoint().await.unwrap().is_none());

        wallet
            .rewind_to_height(ChainState::empty(1.into(), BlockHash([2; 32])))
            .await
            .unwrap();
        let cursor = wallet.treasury_cursor().await.unwrap().unwrap();
        assert_eq!(cursor.height, 1);
        assert_eq!(cursor.block_hash, "02".repeat(32));
    }

    #[tokio::test]
    async fn failed_cursor_commit_rolls_back_reward_import_and_reopen_keeps_progress() {
        let (dir, store, wallet) = setup();
        let receiver = store
            .account(TREASURY_ACCOUNT_ID)
            .unwrap()
            .transparent_address;
        let initial = wallet
            .initialize_treasury_cursor(&receiver, 1, &"01".repeat(32))
            .await
            .unwrap();
        let reward = transaction(&receiver, OutPoint::new([0; 32], u32::MAX), 0);
        let txid = reward.txid();
        let connection = rusqlite::Connection::open(dir.path().join("wallet.db")).unwrap();
        connection.execute_batch("CREATE TRIGGER ext_ths_fail_cursor BEFORE UPDATE ON ext_tsz_treasury_cursor BEGIN SELECT RAISE(ABORT, 'injected cursor failure'); END").unwrap();

        let error = wallet
            .commit_treasury_discovery(&initial, &checkpoint(2, 2), Some(&raw(&reward)), &[])
            .await
            .unwrap_err();
        assert!(error.to_string().contains("injected cursor failure"));
        assert_eq!(
            wallet.treasury_cursor().await.unwrap(),
            Some(initial.clone())
        );
        assert!(
            wallet
                .with_db(move |db| Ok(db.get_transaction(txid)?.is_none()))
                .await
                .unwrap()
        );

        connection
            .execute_batch("DROP TRIGGER ext_ths_fail_cursor")
            .unwrap();
        let advanced = wallet
            .commit_treasury_discovery(&initial, &checkpoint(2, 2), Some(&raw(&reward)), &[])
            .await
            .unwrap();
        drop(wallet);
        let reopened = RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();
        assert_eq!(reopened.treasury_cursor().await.unwrap(), Some(advanced));
        assert!(
            reopened
                .with_db(move |db| Ok(db.get_transaction(txid)?.is_some()))
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn known_spender_must_be_stored_and_spend_the_exact_reward_output() {
        let (_dir, store, wallet) = setup();
        let receiver = store
            .account(TREASURY_ACCOUNT_ID)
            .unwrap()
            .transparent_address;
        let reward = transaction(&receiver, OutPoint::new([0; 32], u32::MAX), 0);
        wallet.enhance_transaction(&raw(&reward), 2).await.unwrap();
        let spender = transaction(&receiver, OutPoint::new(*reward.txid().as_ref(), 0), 1);
        wallet
            .enhance_transaction(&raw(&spender), 103)
            .await
            .unwrap();
        let spenders = wallet
            .known_spending_transactions(&reward.txid().to_string(), 0)
            .await
            .unwrap();
        assert_eq!(spenders, vec![spender.txid().to_string()]);
        assert!(
            wallet
                .known_spending_transactions(&reward.txid().to_string(), 1)
                .await
                .unwrap()
                .is_empty()
        );

        let initial = wallet
            .initialize_treasury_cursor(&receiver, 1, &"01".repeat(32))
            .await
            .unwrap();
        let error = wallet
            .commit_treasury_discovery(
                &initial,
                &checkpoint(2, 2),
                Some(&raw(&reward)),
                &[(0, "09".repeat(32))],
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("spender is not stored"));
        assert_eq!(
            wallet.treasury_cursor().await.unwrap(),
            Some(initial.clone())
        );
        let advanced = wallet
            .commit_treasury_discovery(
                &initial,
                &checkpoint(2, 2),
                Some(&raw(&reward)),
                &[(0, spenders[0].clone())],
            )
            .await
            .unwrap();
        assert_eq!(advanced.height, 2);
    }

    #[tokio::test]
    async fn rewind_updates_sdk_and_cursor_atomically() {
        let (dir, store, wallet) = setup();
        let receiver = store
            .account(TREASURY_ACCOUNT_ID)
            .unwrap()
            .transparent_address;
        use zcash_client_backend::data_api::scanning::ScanPriority;
        wallet.update_chain_tip(2).await.unwrap();
        wallet
            .scan_batch(
                ScanRange::from_parts(2.into()..3.into(), ScanPriority::Historic),
                vec![CompactBlock {
                    height: 2,
                    hash: vec![2; 32],
                    prev_hash: vec![1; 32],
                    chain_metadata: Some(Default::default()),
                    ..Default::default()
                }],
                ChainState::empty(1.into(), BlockHash([1; 32])),
            )
            .await
            .unwrap();
        let initial = wallet
            .initialize_treasury_cursor(&receiver, 1, &"01".repeat(32))
            .await
            .unwrap();
        let reward = transaction(&receiver, OutPoint::new([0; 32], u32::MAX), 0);
        let txid = reward.txid();
        let advanced = wallet
            .commit_treasury_discovery(&initial, &checkpoint(2, 2), Some(&raw(&reward)), &[])
            .await
            .unwrap();
        assert_eq!(
            wallet
                .with_db(move |db| Ok(db.get_tx_height(txid)?))
                .await
                .unwrap(),
            Some(2.into())
        );
        let connection = rusqlite::Connection::open(dir.path().join("wallet.db")).unwrap();
        connection.execute_batch("CREATE TRIGGER ext_ths_fail_rewind BEFORE UPDATE ON ext_tsz_treasury_cursor BEGIN SELECT RAISE(ABORT, 'injected rewind failure'); END").unwrap();
        let state = ChainState::empty(1.into(), BlockHash([1; 32]));
        let error = wallet.rewind_to_height(state.clone()).await.unwrap_err();
        assert!(error.to_string().contains("injected rewind failure"));
        assert_eq!(wallet.treasury_cursor().await.unwrap(), Some(advanced));
        assert_eq!(
            wallet
                .with_db(move |db| Ok(db.get_tx_height(txid)?))
                .await
                .unwrap(),
            Some(2.into())
        );

        connection
            .execute_batch("DROP TRIGGER ext_ths_fail_rewind")
            .unwrap();
        wallet.rewind_to_height(state).await.unwrap();
        let cursor = wallet.treasury_cursor().await.unwrap().unwrap();
        assert_eq!(cursor.height, 1);
        assert_eq!(cursor.block_hash, "01".repeat(32));
        assert_eq!(
            wallet
                .with_db(move |db| Ok(db.get_tx_height(txid)?))
                .await
                .unwrap(),
            None
        );
    }
}
