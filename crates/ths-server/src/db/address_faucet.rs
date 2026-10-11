use super::activity::activity_for_key;
use super::*;

/// The order must match `row_address_faucet`.
const ADDRESS_FAUCET_COLUMNS: &str = "id,address,amount_zatoshi,txid,block_hash,status";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressFaucet {
    pub id: String,
    pub address: String,
    pub amount_zatoshi: u64,
    pub txid: String,
    pub block_hash: Option<String>,
    pub status: String,
}

impl Store {
    pub fn address_faucet_for_key(&self, key: &str) -> Result<Option<AddressFaucet>> {
        let db = self.0.lock().unwrap();
        address_for_key(&db, key)
    }

    pub fn claim_address_faucet(
        &self,
        address: &str,
        amount: u64,
        key: &str,
    ) -> Result<AddressFaucet> {
        if amount == 0 {
            bail!("amount must be greater than zero");
        }
        let db = self.0.lock().unwrap();
        if let Some(payment) = address_for_key(&db, key)? {
            if payment.address != address || payment.amount_zatoshi != amount {
                return Err(IdempotencyConflict.into());
            }
            return Ok(payment);
        }
        if activity_for_key(&db, key)?.is_some() {
            return Err(IdempotencyConflict.into());
        }
        let id = Uuid::new_v4().to_string();
        db.execute(
            "INSERT INTO address_faucets(key,id,address,amount_zatoshi) VALUES(?1,?2,?3,?4)",
            params![key, id, address, amount],
        )?;
        address_for_key(&db, key)?.context("address faucet claim was not stored")
    }

    pub fn discard_address_preparing(&self, id: &str) -> Result<()> {
        self.0.lock().unwrap().execute(
            "DELETE FROM address_faucets WHERE id=?1 AND status='preparing'",
            [id],
        )?;
        Ok(())
    }

    pub fn record_address_prepared(
        &self,
        id: &str,
        txid: &str,
        raw_transaction: &[u8],
        expiry_height: u64,
    ) -> Result<AddressFaucet> {
        let mut db = self.0.lock().unwrap();
        let tx = db.transaction()?;
        let updated = tx.execute(
            "UPDATE address_faucets SET txid=?1,status='prepared' WHERE id=?2 AND status='preparing'",
            params![txid, id],
        )?;
        let payment = address_by_id(&tx, id)?;
        if updated == 0
            && (payment.txid != txid
                || !matches!(
                    payment.status.as_str(),
                    "prepared" | "broadcast" | "confirmed"
                ))
        {
            bail!("address faucet {id} is not waiting for this prepared transaction");
        }
        tx.execute(
            "INSERT INTO prepared_payments(activity_id,raw_transaction,expiry_height) VALUES(?1,?2,?3) ON CONFLICT(activity_id) DO NOTHING",
            params![id, raw_transaction, expiry_height],
        )?;
        tx.commit()?;
        Ok(payment)
    }

    pub fn reset_address_for_retry(&self, id: &str, txid: &str) -> Result<AddressFaucet> {
        let mut db = self.0.lock().unwrap();
        let tx = db.transaction()?;
        if tx.execute(
            "UPDATE address_faucets SET txid='',block_hash=NULL,status='preparing' WHERE id=?1 AND txid=?2 AND status IN ('prepared','broadcast')",
            params![id, txid],
        )? == 1 {
            tx.execute("DELETE FROM prepared_payments WHERE activity_id=?1", [id])?;
        }
        let payment = address_by_id(&tx, id)?;
        tx.commit()?;
        Ok(payment)
    }

    pub fn mark_address_broadcast(&self, id: &str, txid: &str) -> Result<AddressFaucet> {
        let db = self.0.lock().unwrap();
        let updated = db.execute(
            "UPDATE address_faucets SET status='broadcast' WHERE id=?1 AND txid=?2 AND status IN ('prepared','broadcast')",
            params![id, txid],
        )?;
        let payment = address_by_id(&db, id)?;
        if updated == 0 && (payment.txid != txid || payment.status != "confirmed") {
            bail!("address faucet {id} has no matching prepared transaction");
        }
        Ok(payment)
    }

    pub fn confirm_address(&self, id: &str, txid: &str, block_hash: &str) -> Result<AddressFaucet> {
        if block_hash.is_empty() {
            bail!("block hash is required to confirm address faucet");
        }
        let db = self.0.lock().unwrap();
        db.execute(
            "UPDATE address_faucets SET status='confirmed',block_hash=?1 WHERE id=?2 AND txid=?3",
            params![block_hash, id, txid],
        )?;
        address_by_id(&db, id)
    }
}

pub(super) fn address_for_key(db: &Connection, key: &str) -> Result<Option<AddressFaucet>> {
    db.query_row(
        &format!("SELECT {ADDRESS_FAUCET_COLUMNS} FROM address_faucets WHERE key=?1"),
        [key],
        row_address_faucet,
    )
    .optional()
    .map_err(Into::into)
}
fn address_by_id(db: &Connection, id: &str) -> Result<AddressFaucet> {
    db.query_row(
        &format!("SELECT {ADDRESS_FAUCET_COLUMNS} FROM address_faucets WHERE id=?1"),
        [id],
        row_address_faucet,
    )
    .map_err(Into::into)
}
fn row_address_faucet(row: &rusqlite::Row<'_>) -> rusqlite::Result<AddressFaucet> {
    Ok(AddressFaucet {
        id: row.get(0)?,
        address: row.get(1)?,
        amount_zatoshi: row.get(2)?,
        txid: row.get(3)?,
        block_hash: row.get(4)?,
        status: row.get(5)?,
    })
}

#[cfg(test)]
#[path = "../../tests/unit/db/address_faucet/tests.rs"]
mod tests;
