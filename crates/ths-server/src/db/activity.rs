use super::address_faucet::address_for_key;
use super::*;

/// Qualified so it also reads unambiguously in joins. The order must match `row_activity`.
const ACTIVITY_COLUMNS: &str = "a.id,a.kind,a.from_account,a.to_account,a.source_pool,a.destination_pool,a.amount_zatoshi,a.txid,a.block_hash,a.status,a.created_at";
#[derive(Debug, Clone, Serialize)]
pub struct Activity {
    pub id: String,
    pub kind: String,
    pub from_account: Option<u8>,
    pub to_account: u8,
    pub source_pool: String,
    pub destination_pool: String,
    pub amount_zatoshi: u64,
    pub txid: String,
    pub block_hash: Option<String>,
    pub status: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedTransaction {
    pub raw_transaction: Vec<u8>,
    pub expiry_height: u64,
}

impl Store {
    pub fn activities(&self, limit: u32) -> Result<Vec<Activity>> {
        let db = self.0.lock().unwrap();
        let mut query = db.prepare(&format!(
            "SELECT {ACTIVITY_COLUMNS} FROM activity a WHERE a.txid != '' ORDER BY a.rowid DESC LIMIT ?1"
        ))?;
        Ok(query
            .query_map([limit.min(100)], row_activity)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn unconfirmed_activities(&self) -> Result<Vec<Activity>> {
        let db = self.0.lock().unwrap();
        let mut query = db.prepare(&format!(
            "SELECT {ACTIVITY_COLUMNS} FROM activity a WHERE a.txid!='' AND a.status!='confirmed' ORDER BY a.rowid ASC"
        ))?;
        Ok(query
            .query_map([], row_activity)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn activity_for_key(&self, key: &str) -> Result<Option<Activity>> {
        let db = self.0.lock().unwrap();
        activity_for_key(&db, key)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn claim_transfer(
        &self,
        from: u8,
        to: u8,
        source_pool: &str,
        destination_pool: &str,
        amount: u64,
        key: &str,
        memo: Option<&str>,
    ) -> Result<Activity> {
        validate_pool(source_pool)?;
        validate_pool(destination_pool)?;
        let request = new_activity(
            "send",
            Some(from),
            to,
            source_pool,
            destination_pool,
            amount,
        );
        self.claim(request, key, memo)
    }

    pub fn claim_faucet(&self, to: u8, pool: &str, amount: u64, key: &str) -> Result<Activity> {
        validate_pool(pool)?;
        let request = new_activity("faucet", None, to, "ironwood", pool, amount);
        self.claim(request, key, None)
    }

    /// Returns the payment already claimed with `key`, or records `request` as a new
    /// `preparing` claim. Submission and recovery happen outside this function.
    fn claim(&self, request: Activity, key: &str, memo: Option<&str>) -> Result<Activity> {
        if request.amount_zatoshi == 0 {
            bail!("amount must be greater than zero");
        }
        // Hold the lock from lookup to insert so concurrent retries create one payment.
        let mut db = self.0.lock().unwrap();
        if let Some(activity) = activity_for_key(&db, key)? {
            let saved_memo: Option<String> =
                db.query_row("SELECT memo FROM idempotency WHERE key=?1", [key], |row| {
                    row.get(0)
                })?;
            // An absent memo and an empty memo are different payments.
            if !same_payment(&activity, &request) || saved_memo.as_deref() != memo {
                return Err(IdempotencyConflict.into());
            }
            return Ok(activity);
        }
        if address_for_key(&db, key)?.is_some() {
            return Err(IdempotencyConflict.into());
        }
        for id in request.from_account.into_iter().chain([request.to_account]) {
            if !db.query_row(
                "SELECT EXISTS(SELECT 1 FROM accounts WHERE id=?1)",
                [id],
                |row| row.get::<_, bool>(0),
            )? {
                bail!("account {id} does not exist");
            }
        }
        let tx = db.transaction()?;
        insert_activity(&tx, &request, key, memo)?;
        tx.commit()?;
        Ok(request)
    }

    pub fn discard_preparing(&self, id: &str) -> Result<()> {
        let mut db = self.0.lock().unwrap();
        let tx = db.transaction()?;
        if tx.execute(
            "DELETE FROM activity WHERE id=?1 AND status='preparing'",
            [id],
        )? == 1
        {
            tx.execute("DELETE FROM idempotency WHERE activity_id=?1", [id])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn record_prepared(
        &self,
        id: &str,
        txid: &str,
        raw_transaction: &[u8],
        expiry_height: u64,
    ) -> Result<Activity> {
        let mut db = self.0.lock().unwrap();
        let tx = db.transaction()?;
        let updated = tx.execute(
            "UPDATE activity SET txid=?1,status='prepared' WHERE id=?2 AND status='preparing'",
            params![txid, id],
        )?;
        let activity = activity_by_id(&tx, id)?;
        if updated == 0
            && (activity.txid != txid
                || !matches!(
                    activity.status.as_str(),
                    "prepared" | "broadcast" | "confirmed"
                ))
        {
            bail!("payment {id} is not waiting for this prepared transaction");
        }
        tx.execute(
            "INSERT INTO prepared_payments(activity_id,raw_transaction,expiry_height) VALUES(?1,?2,?3) ON CONFLICT(activity_id) DO NOTHING",
            params![id, raw_transaction, expiry_height],
        )?;
        tx.commit()?;
        Ok(activity)
    }

    pub fn prepared_transaction(&self, id: &str) -> Result<PreparedTransaction> {
        self.0
            .lock()
            .unwrap()
            .query_row(
                "SELECT raw_transaction,expiry_height FROM prepared_payments WHERE activity_id=?1",
                [id],
                |row| {
                    Ok(PreparedTransaction {
                        raw_transaction: row.get(0)?,
                        expiry_height: row.get(1)?,
                    })
                },
            )
            .with_context(|| format!("prepared transaction {id} is missing"))
    }

    pub fn reset_for_retry(&self, id: &str, txid: &str) -> Result<Activity> {
        let mut db = self.0.lock().unwrap();
        let tx = db.transaction()?;
        let updated = tx.execute(
            "UPDATE activity SET txid='',block_hash=NULL,status='preparing' WHERE id=?1 AND txid=?2 AND status IN ('prepared','broadcast')",
            params![id, txid],
        )?;
        if updated == 1 {
            tx.execute("DELETE FROM prepared_payments WHERE activity_id=?1", [id])?;
        }
        let activity = activity_by_id(&tx, id)?;
        tx.commit()?;
        Ok(activity)
    }

    pub fn mark_broadcast(&self, id: &str, txid: &str) -> Result<Activity> {
        let db = self.0.lock().unwrap();
        let updated = db.execute(
            "UPDATE activity SET status='broadcast' WHERE id=?1 AND txid=?2 AND status IN ('prepared','broadcast')",
            params![id, txid],
        )?;
        let activity = activity_by_id(&db, id)?;
        if updated == 0 && (activity.txid != txid || activity.status != "confirmed") {
            bail!("payment {id} has no matching prepared transaction");
        }
        Ok(activity)
    }

    pub fn confirm(&self, id: &str, txid: &str, block_hash: &str) -> Result<Activity> {
        if block_hash.is_empty() {
            bail!("block hash is required to confirm activity");
        }
        let db = self.0.lock().unwrap();
        db.execute(
            "UPDATE activity SET status='confirmed',block_hash=?1 WHERE id=?2 AND txid=?3",
            params![block_hash, id, txid],
        )?;
        activity_by_id(&db, id).map_err(Into::into)
    }
}

fn insert_activity(db: &Connection, a: &Activity, key: &str, memo: Option<&str>) -> Result<()> {
    db.execute("INSERT INTO activity(id,kind,from_account,to_account,source_pool,destination_pool,amount_zatoshi,txid,status) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)", params![a.id,a.kind,a.from_account,a.to_account,a.source_pool,a.destination_pool,a.amount_zatoshi,a.txid,a.status])?;
    db.execute(
        "INSERT INTO idempotency(key,activity_id,memo) VALUES(?1,?2,?3)",
        params![key, a.id, memo],
    )?;
    Ok(())
}

fn activity_by_id(db: &Connection, id: &str) -> rusqlite::Result<Activity> {
    db.query_row(
        &format!("SELECT {ACTIVITY_COLUMNS} FROM activity a WHERE a.id=?1"),
        [id],
        row_activity,
    )
}

pub(super) fn activity_for_key(db: &Connection, key: &str) -> Result<Option<Activity>> {
    db.query_row(
        &format!(
            "SELECT {ACTIVITY_COLUMNS} FROM activity a JOIN idempotency i ON i.activity_id=a.id WHERE i.key=?1"
        ),
        [key],
        row_activity,
    )
    .optional()
    .map_err(Into::into)
}
fn new_activity(
    kind: &str,
    from: Option<u8>,
    to: u8,
    source: &str,
    destination: &str,
    amount: u64,
) -> Activity {
    let id = Uuid::new_v4().to_string();
    Activity {
        id,
        kind: kind.into(),
        from_account: from,
        to_account: to,
        source_pool: source.into(),
        destination_pool: destination.into(),
        amount_zatoshi: amount,
        txid: String::new(),
        block_hash: None,
        status: "preparing".into(),
        created_at: String::new(),
    }
}
fn validate_pool(pool: &str) -> Result<()> {
    if matches!(pool, "transparent" | "ironwood") {
        Ok(())
    } else {
        bail!("pool must be transparent or ironwood")
    }
}

fn same_payment(existing: &Activity, request: &Activity) -> bool {
    existing.kind == request.kind
        && existing.from_account == request.from_account
        && existing.to_account == request.to_account
        && existing.source_pool == request.source_pool
        && existing.destination_pool == request.destination_pool
        && existing.amount_zatoshi == request.amount_zatoshi
}
fn row_activity(row: &rusqlite::Row<'_>) -> rusqlite::Result<Activity> {
    Ok(Activity {
        id: row.get(0)?,
        kind: row.get(1)?,
        from_account: row.get(2)?,
        to_account: row.get(3)?,
        source_pool: row.get(4)?,
        destination_pool: row.get(5)?,
        amount_zatoshi: row.get(6)?,
        txid: row.get(7)?,
        block_hash: row.get(8)?,
        status: row.get(9)?,
        created_at: row.get(10)?,
    })
}

#[cfg(test)]
#[path = "../../tests/unit/db/activity/tests.rs"]
mod tests;
