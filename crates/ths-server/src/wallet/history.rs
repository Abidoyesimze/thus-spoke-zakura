use super::RealWallet;
use crate::db::USER_ACCOUNT_COUNT;
use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, params};
use std::collections::BTreeSet;
use uuid::Uuid;
use zcash_protocol::{
    TxId,
    memo::{Memo, MemoBytes},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CanonicalActivity {
    pub(crate) direction: &'static str,
    pub(crate) from_account: Option<u8>,
    pub(crate) to_account: Option<u8>,
    pub(crate) recipient_addresses: Vec<String>,
    pub(crate) source_pools: Vec<String>,
    pub(crate) destination_pools: Vec<String>,
    pub(crate) amount_zatoshi: u64,
    pub(crate) txid: String,
    pub(crate) block_hash: Option<String>,
    pub(crate) mined_height: Option<u32>,
    pub(crate) tx_index: Option<u32>,
    pub(crate) confirmations: u32,
    pub(crate) created_at: Option<String>,
    pub(crate) memos: Vec<String>,
}

#[derive(Clone)]
struct Output {
    pool: Option<String>,
    from_account: Option<u8>,
    to_account: Option<u8>,
    address: Option<String>,
    is_change: bool,
    value: u64,
    memo: Option<String>,
}

fn pool_name(code: i64) -> Option<String> {
    match code {
        0 => Some("transparent".to_owned()),
        4 => Some("ironwood".to_owned()),
        _ => None,
    }
}

fn text_memo(bytes: Option<Vec<u8>>) -> Option<String> {
    let bytes: [u8; 512] = bytes?.try_into().ok()?;
    match Memo::try_from(MemoBytes::from_bytes(&bytes).ok()?).ok()? {
        Memo::Text(text) => Some(String::from(&*text)),
        Memo::Empty | Memo::Arbitrary(_) | Memo::Future(_) => None,
    }
}

fn user_account(account_ids: &[Uuid], uuid: Option<Uuid>) -> Option<u8> {
    uuid.and_then(|uuid| {
        account_ids
            .iter()
            .position(|candidate| *candidate == uuid)
            .and_then(|index| u8::try_from(index + 1).ok())
            .filter(|id| *id <= USER_ACCOUNT_COUNT)
    })
}

fn unique<T: Ord>(values: impl IntoIterator<Item = T>) -> Vec<T> {
    values
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn only<T: Copy>(values: &[T]) -> Option<T> {
    match values {
        [value] => Some(*value),
        _ => None,
    }
}

fn outputs_for(db: &Connection, txid: &[u8; 32], account_ids: &[Uuid]) -> Result<Vec<Output>> {
    let mut query = db.prepare(
        "SELECT output_pool,from_account_uuid,to_account_uuid,to_address,is_change,value,memo
         FROM v_tx_outputs WHERE txid=?1 ORDER BY output_pool,output_index",
    )?;
    Ok(query
        .query_map([txid], |row| {
            Ok(Output {
                pool: pool_name(row.get(0)?),
                from_account: user_account(account_ids, row.get(1)?),
                to_account: user_account(account_ids, row.get(2)?),
                address: row.get(3)?,
                is_change: row.get(4)?,
                value: row.get(5)?,
                memo: text_memo(row.get(6)?),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

fn source_pools_for(db: &Connection, txid: &[u8; 32], account_ids: &[Uuid]) -> Result<Vec<String>> {
    let mut query = db.prepare(
        "SELECT DISTINCT ro.pool
         FROM v_received_output_spends ros
         JOIN v_received_outputs ro
           ON ro.pool=ros.pool AND ro.id_within_pool_table=ros.received_output_id
         JOIN transactions t ON t.id_tx=ros.transaction_id
         JOIN accounts a ON a.id=ros.account_id
         WHERE t.txid=?1 AND a.uuid IN (?2,?3,?4,?5,?6) ORDER BY ro.pool",
    )?;
    let pools = query
        .query_map(
            params![
                txid,
                account_ids[0],
                account_ids[1],
                account_ids[2],
                account_ids[3],
                account_ids[4],
            ],
            |row| row.get::<_, i64>(0),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(pools.into_iter().filter_map(pool_name).collect())
}

fn read_canonical_activity(
    db: &Connection,
    account_ids: &[Uuid],
    limit: u32,
) -> Result<Vec<CanonicalActivity>> {
    let tip: Option<u32> = db.query_row("SELECT MAX(height) FROM blocks", [], |row| row.get(0))?;
    let mut query = db.prepare(
        "SELECT DISTINCT vt.txid,vt.mined_height,vt.tx_index,
                b.hash,t.created
         FROM v_transactions vt
         JOIN transactions t ON t.txid=vt.txid
         LEFT JOIN blocks b ON b.height=vt.mined_height
         WHERE account_uuid IN (?1,?2,?3,?4,?5)
         ORDER BY vt.mined_height DESC,vt.tx_index DESC LIMIT ?6",
    )?;
    let mut rows = query.query(params![
        account_ids[0],
        account_ids[1],
        account_ids[2],
        account_ids[3],
        account_ids[4],
        limit.min(100),
    ])?;
    let mut summaries = Vec::new();
    while let Some(row) = rows.next()? {
        let txid_bytes: [u8; 32] = row.get(0)?;
        let mined_height: Option<u32> = row.get(1)?;
        let tx_index: Option<u32> = row.get(2)?;
        let block_hash = row
            .get::<_, Option<[u8; 32]>>(3)?
            .map(|hash| zcash_primitives::block::BlockHash(hash).to_string());
        // `transactions.created` is only available when this wallet created
        // the transaction. An incoming shielded payment does not reveal the
        // sender's creation time; the Regtest block clock is not a substitute.
        let created_at: Option<String> = row.get(4)?;
        let outputs = outputs_for(db, &txid_bytes, account_ids)?;
        let relevant: Vec<_> = outputs
            .iter()
            .filter(|output| {
                !output.is_change
                    || matches!(
                        (output.from_account, output.to_account),
                        (Some(from), Some(to)) if from != to
                    )
            })
            .collect();
        let source_pools = source_pools_for(db, &txid_bytes, account_ids)?;
        let destination_pools = unique(relevant.iter().filter_map(|output| output.pool.clone()));
        let from_accounts = unique(relevant.iter().filter_map(|output| output.from_account));
        let to_accounts = unique(relevant.iter().filter_map(|output| output.to_account));
        let recipient_addresses =
            unique(relevant.iter().filter_map(|output| output.address.clone()));
        let memos = unique(relevant.iter().filter_map(|output| output.memo.clone()));
        let from_account = only(&from_accounts);
        let to_account = only(&to_accounts);
        let direction = match (from_account, to_account) {
            (Some(from), Some(to)) if from == to => "self",
            (Some(_), Some(_)) => "internal",
            (Some(_), None) => "outgoing",
            (None, Some(_)) => "incoming",
            (None, None) => "unknown",
        };
        let amount_zatoshi = relevant.iter().try_fold(0u64, |total, output| {
            total
                .checked_add(output.value)
                .context("wallet transaction output value overflowed")
        })?;
        let confirmations = mined_height
            .zip(tip)
            .map(|(height, tip)| tip.saturating_sub(height).saturating_add(1))
            .unwrap_or(0);
        summaries.push(CanonicalActivity {
            direction,
            from_account,
            to_account,
            recipient_addresses,
            source_pools,
            destination_pools,
            amount_zatoshi,
            txid: TxId::from_bytes(txid_bytes).to_string(),
            block_hash,
            mined_height,
            tx_index,
            confirmations,
            created_at,
            memos,
        });
    }
    Ok(summaries)
}

impl RealWallet {
    pub(crate) async fn canonical_activity(&self, limit: u32) -> Result<Vec<CanonicalActivity>> {
        let path = self.wallet_path.clone();
        let account_ids = self
            .account_ids
            .iter()
            .map(|id| id.expose_uuid())
            .collect::<Vec<_>>();
        self.with_db(move |_| {
            let db = Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            read_canonical_activity(&db, &account_ids, limit)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::only;

    #[test]
    fn one_account_is_optional_without_indexing_an_empty_list() {
        assert_eq!(only::<u8>(&[]), None);
        assert_eq!(only(&[2]), Some(2));
        assert_eq!(only(&[1, 2]), None);
    }
}
