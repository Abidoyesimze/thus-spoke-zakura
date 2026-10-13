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
#[path = "../../tests/unit/wallet/recovery/tests.rs"]
mod tests;
