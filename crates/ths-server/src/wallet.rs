use std::{
    collections::{BTreeMap, HashSet},
    convert::Infallible,
    io,
    path::Path,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use rand10::{rand_core::UnwrapErr, rngs::SysRng};
use rusqlite::OptionalExtension;
use schemerz_rusqlite::RusqliteMigration;
use secrecy::{ExposeSecret, SecretVec};
use serde::Serialize;
use tokio::sync::Mutex;
use uuid::Uuid;
use zcash_client_backend::{
    data_api::{
        Account as _, AccountBirthday, CoinbaseFilter, InputSource, MaxSpendMode, TargetValue,
        WalletCommitmentTrees, WalletRead, WalletWrite,
        chain::{BlockSource, ChainState, CommitmentTreeRoot, error, scan_cached_blocks},
        error::Error as WalletError,
        scanning::ScanRange,
        wallet::{
            ConfirmationsPolicy, SpendingKeys, create_proposed_transactions,
            decrypt_and_store_transaction,
            input_selection::{
                GreedyInputSelector, LockFilter, LockedInputPolicy, SpendPolicy,
                TransparentSpendPolicy,
            },
            propose_send_max_transfer, propose_shielding_coinbase,
            propose_standard_transfer_to_address, propose_transfer,
        },
    },
    fees::{DustOutputPolicy, StandardFeeRule, standard::SingleOutputChangeStrategy},
    proposal::Proposal,
    proto::{
        compact_formats::CompactBlock,
        service::{
            ChainSpec, RawTransaction, SendResponse,
            compact_tx_streamer_client::CompactTxStreamerClient,
        },
    },
    wallet::{OvkPolicy, WalletTransparentOutput},
};
use zcash_client_sqlite::{
    AccountUuid, WalletDb,
    util::SystemClock,
    wallet::init::{WalletMigrationError, WalletMigrator, migrations},
};
use zcash_keys::{address::Address, encoding::AddressCodec, keys::UnifiedSpendingKey};
use zcash_primitives::block::BlockHash;
use zcash_primitives::merkle_tree::HashSer;
use zcash_primitives::transaction::Transaction;
use zcash_proofs::prover::LocalTxProver;
use zcash_protocol::{
    ShieldedPool, TxId,
    consensus::{BlockHeight, BranchId},
    local_consensus::LocalNetwork,
    memo::MemoBytes,
    value::Zatoshis,
};
use zip321::{Payment, TransactionRequest};

use crate::db::{Account, ZATOSHIS_PER_ZEC};
use transparent::{
    address::Script,
    bundle::{OutPoint, TxOut},
};
use zcash_script::script;

use crate::{network::regtest_network, rpc::ChainCheckpoint};

mod journal;
mod payments;
mod recovery;
mod submission;

pub use journal::PreparedPayment;
use journal::{PreparedPaymentsMigration, prepared_payment};
pub use payments::{PaymentError, SendQuote};
#[cfg(test)]
pub(crate) use submission::check_broadcast_response;

type Db = WalletDb<rusqlite::Connection, LocalNetwork, SystemClock, UnwrapErr<SysRng>>;
pub(crate) const WALLET_BIRTHDAY_HEIGHT: u32 = 2;

#[derive(Clone, Debug)]
pub(crate) struct TransparentQuery {
    pub account_id: AccountUuid,
    pub addresses: Vec<String>,
    pub start_height: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct TransparentOutputData {
    pub txid: [u8; 32],
    pub index: u32,
    pub value_zatoshi: i64,
    pub script: Vec<u8>,
    pub height: u32,
    pub account_id: AccountUuid,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ShieldedProtocol {
    Sapling,
    Orchard,
    Ironwood,
}

#[derive(Clone, Debug)]
pub(crate) struct SubtreeRootData {
    pub completing_height: u32,
    pub root_hash: Vec<u8>,
}

fn to_roots<H: HashSer>(roots: Vec<SubtreeRootData>) -> Result<Vec<CommitmentTreeRoot<H>>> {
    roots
        .into_iter()
        .map(|root| {
            Ok(CommitmentTreeRoot::from_parts(
                BlockHeight::from_u32(root.completing_height),
                H::read(&root.root_hash[..])?,
            ))
        })
        .collect()
}

pub(crate) enum ScanOutcome {
    Scanned(bool),
    Rewind(u32),
}

#[derive(Clone)]
pub struct RealWallet {
    db: Arc<Mutex<Db>>,
    account_ids: Vec<AccountUuid>,
    lightwalletd: String,
}

struct MemoryBlockCache(BTreeMap<u32, CompactBlock>);

impl BlockSource for MemoryBlockCache {
    type Error = io::Error;

    fn with_blocks<F, E>(
        &self,
        from: Option<BlockHeight>,
        limit: Option<usize>,
        mut f: F,
    ) -> Result<(), error::Error<E, Self::Error>>
    where
        F: FnMut(CompactBlock) -> Result<(), error::Error<E, Self::Error>>,
    {
        let from = from.map(u32::from).unwrap_or(0);
        for block in self.0.range(from..).take(limit.unwrap_or(usize::MAX)) {
            f(block.1.clone())?;
        }
        Ok(())
    }
}

impl RealWallet {
    pub fn open(data_dir: &Path, seed_hex: &str) -> Result<Self> {
        let seed = hex::decode(seed_hex).context("invalid wallet seed")?;
        let secret = SecretVec::new(seed);
        let wallet_path = data_dir.join("wallet.db");
        let mut db = WalletDb::for_path(
            wallet_path,
            regtest_network(),
            SystemClock,
            UnwrapErr(SysRng),
        )?;
        WalletMigrator::new()
            .with_seed(SecretVec::new(secret.expose_secret().clone()))
            .with_external_migrations(vec![
                Box::new(PreparedPaymentsMigration),
                Box::new(recovery::TreasuryCursorMigration),
            ])
            .init_or_migrate(&mut db)
            .map_err(|e| anyhow::anyhow!("initializing wallet database: {e}"))?;

        let account_count = db.get_account_ids()?.len();
        if account_count == 0 || account_count == usize::from(crate::db::USER_ACCOUNT_COUNT) {
            // lightwalletd treats a BlockId with height 0 as unspecified, while the
            // SDK asks for the tree state immediately before an account birthday.
            // Start at block 2 so that the initial tree-state request is for block 1.
            // Block 1 is an expendable mining-reward block on this local regtest.
            let birthday = AccountBirthday::from_parts(
                ChainState::empty(
                    BlockHeight::from_u32(WALLET_BIRTHDAY_HEIGHT - 1),
                    BlockHash([0; 32]),
                ),
                None,
            );
            for id in (account_count + 1)..=usize::from(crate::db::TREASURY_ACCOUNT_ID) {
                db.create_account(&format!("Account {id}"), &secret, &birthday, None)?;
            }
        }
        if db.get_account_ids()?.len() != usize::from(crate::db::TREASURY_ACCOUNT_ID) {
            bail!("wallet database must contain exactly six accounts");
        }
        let mut accounts = db
            .get_account_ids()?
            .into_iter()
            .map(|id| {
                db.get_account(id)?
                    .map(|account| (account.name().unwrap_or_default().to_owned(), id))
                    .context("wallet account disappeared")
            })
            .collect::<Result<Vec<_>>>()?;
        accounts.sort_by(|a, b| a.0.cmp(&b.0));
        let account_ids = accounts.into_iter().map(|(_, id)| id).collect();

        Ok(Self {
            db: Arc::new(Mutex::new(db)),
            account_ids,
            lightwalletd: std::env::var("THS_LIGHTWALLETD")
                .unwrap_or_else(|_| "http://127.0.0.1:9067".into()),
        })
    }

    pub(crate) fn lightwalletd(&self) -> &str {
        &self.lightwalletd
    }

    async fn with_db<T, F>(&self, operation: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Db) -> Result<T> + Send + 'static,
    {
        let mut db = Arc::clone(&self.db).lock_owned().await;
        tokio::task::spawn_blocking(move || operation(&mut db))
            .await
            .context("wallet worker stopped")?
    }

    pub(crate) async fn public_transparent_queries(&self) -> Result<Vec<TransparentQuery>> {
        let account_ids = self.account_ids[..usize::from(crate::db::USER_ACCOUNT_COUNT)].to_vec();
        self.with_db(move |db| {
            account_ids
                .into_iter()
                .map(|account_id| {
                    Ok(TransparentQuery {
                        account_id,
                        addresses: db
                            .get_transparent_receivers(account_id, true, true)?
                            .into_keys()
                            .map(|address| address.encode(&regtest_network()))
                            .collect(),
                        start_height: u64::from(u32::from(db.utxo_query_height(account_id)?)),
                    })
                })
                .collect()
        })
        .await
    }

    pub(crate) async fn insert_transparent_outputs(
        &self,
        outputs: Vec<TransparentOutputData>,
    ) -> Result<()> {
        self.with_db(move |db| {
            for output in outputs {
                let output = WalletTransparentOutput::from_parts(
                    OutPoint::new(output.txid, output.index),
                    TxOut::new(
                        Zatoshis::from_nonnegative_i64(output.value_zatoshi)
                            .map_err(|_| anyhow::anyhow!("negative transparent output value"))?,
                        Script(script::Code(output.script)),
                    ),
                    Some(BlockHeight::from_u32(output.height)),
                    Some(output.account_id),
                    None,
                    None,
                )
                .context("invalid transparent output")?;
                db.put_received_transparent_utxo(&output)?;
            }
            Ok(())
        })
        .await
    }

    pub(crate) async fn put_subtree_roots(
        &self,
        protocol: ShieldedProtocol,
        roots: Vec<SubtreeRootData>,
    ) -> Result<()> {
        self.with_db(move |db| {
            match protocol {
                ShieldedProtocol::Sapling => {
                    db.put_sapling_subtree_roots(0, &to_roots::<sapling::Node>(roots)?)?
                }
                ShieldedProtocol::Orchard => db.put_orchard_subtree_roots(
                    0,
                    &to_roots::<orchard::tree::MerkleHashOrchard>(roots)?,
                )?,
                ShieldedProtocol::Ironwood => db.put_ironwood_subtree_roots(
                    0,
                    &to_roots::<orchard::tree::MerkleHashOrchard>(roots)?,
                )?,
            }
            Ok(())
        })
        .await
    }

    pub(crate) async fn update_chain_tip(&self, height: u64) -> Result<()> {
        let height = u32::try_from(height).context("chain height exceeds the wallet range")?;
        self.with_db(move |db| {
            db.update_chain_tip(BlockHeight::from_u32(height))?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn suggested_scan_ranges(&self) -> Result<Vec<ScanRange>> {
        self.with_db(|db| db.suggest_scan_ranges().map_err(Into::into))
            .await
    }

    pub(crate) async fn scan_batch(
        &self,
        range: ScanRange,
        blocks: Vec<CompactBlock>,
        chain_state: ChainState,
    ) -> Result<ScanOutcome> {
        self.with_db(move |db| {
            let cache = MemoryBlockCache(
                blocks
                    .into_iter()
                    .map(|block| (block.height as u32, block))
                    .collect(),
            );
            match scan_cached_blocks(
                &regtest_network(),
                &cache,
                db,
                range.block_range().start,
                &chain_state,
                range.len(),
            ) {
                Err(error::Error::Scan(error)) if error.is_continuity_error() => {
                    Ok(ScanOutcome::Rewind(
                        u32::from(error.at_height().saturating_sub(10))
                            .max(WALLET_BIRTHDAY_HEIGHT - 1),
                    ))
                }
                Ok(_) => Ok(ScanOutcome::Scanned(
                    db.suggest_scan_ranges()?
                        .first()
                        .is_some_and(|latest| latest.priority() > range.priority()),
                )),
                Err(error) => Err(anyhow::anyhow!("scanning compact blocks: {error}")),
            }
        })
        .await
    }

    pub(crate) async fn rewind_to_height(&self, chain_state: ChainState) -> Result<()> {
        self.with_db(move |db| {
            db.transactionally_with_extension::<_, _, anyhow::Error>(|wallet, ext| {
                let height = u32::from(chain_state.block_height());
                let hash = chain_state.block_hash().to_string();
                wallet.truncate_to_chain_state(chain_state)?;
                ext.execute(
                    "UPDATE ext_tsz_treasury_cursor SET height=?1,block_hash=?2,updated_at=CURRENT_TIMESTAMP WHERE height>=?1",
                    rusqlite::params![height, hash],
                )?;
                Ok(())
            })
        })
        .await
    }

    pub(crate) async fn block_hash(&self, height: u32) -> Result<Option<String>> {
        self.with_db(move |db| {
            Ok(db
                .get_block_hash(height.into())?
                .map(|hash| hash.to_string()))
        })
        .await
    }

    pub(crate) async fn max_scanned_checkpoint(&self) -> Result<Option<ChainCheckpoint>> {
        self.with_db(|db| {
            Ok(db.block_max_scanned()?.map(|block| ChainCheckpoint {
                height: u64::from(u32::from(block.block_height())),
                hash: block.block_hash().to_string(),
            }))
        })
        .await
    }

    pub(crate) async fn scanned_checkpoint(&self) -> Result<Option<ChainCheckpoint>> {
        self.with_db(|db| {
            Ok(db.block_fully_scanned()?.map(|block| ChainCheckpoint {
                height: u64::from(u32::from(block.block_height())),
                hash: block.block_hash().to_string(),
            }))
        })
        .await
    }

    pub(crate) async fn latest_checkpoint(&self) -> Result<ChainCheckpoint> {
        let query = async {
            let mut client = CompactTxStreamerClient::connect(self.lightwalletd.clone()).await?;
            let tip = client
                .get_latest_block(ChainSpec::default())
                .await?
                .into_inner();
            let hash = BlockHash::try_from_slice(&tip.hash)
                .context("lightwalletd returned an invalid tip hash")?;
            Ok::<_, anyhow::Error>(ChainCheckpoint {
                height: tip.height,
                hash: hash.to_string(),
            })
        };
        tokio::time::timeout(Duration::from_secs(5), query)
            .await
            .context("reading lightwalletd tip timed out")?
    }

    pub async fn wait_for_height(&self, target: u64, timeout: Duration) -> Result<()> {
        let mut latest_indexed = None;
        let wait = async {
            let mut client = CompactTxStreamerClient::connect(self.lightwalletd.clone()).await?;
            loop {
                let indexed = client
                    .get_latest_block(ChainSpec::default())
                    .await?
                    .into_inner()
                    .height;
                latest_indexed = Some(indexed);
                if indexed >= target {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        };
        if let Ok(result) = tokio::time::timeout(timeout, wait).await {
            return result;
        }
        let latest_indexed =
            latest_indexed.map_or_else(|| "unknown".into(), |height| height.to_string());
        bail!(
            "lightwalletd did not index Zakura height {target} within {} seconds (latest indexed height: {latest_indexed})",
            timeout.as_secs()
        );
    }

    pub async fn apply_balances(&self, accounts: &mut [Account]) -> Result<()> {
        let Some(summary) = self
            .with_db(|db| Ok(db.get_wallet_summary(ConfirmationsPolicy::MIN)?))
            .await?
        else {
            return Ok(());
        };
        for (account, wallet_id) in accounts.iter_mut().zip(&self.account_ids) {
            if let Some(balance) = summary.account_balances().get(wallet_id) {
                account.transparent_zatoshi = u64::from(balance.unshielded_balance().total());
                account.ironwood_zatoshi = u64::from(balance.ironwood_balance().total());
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub async fn enhance_transaction(&self, raw_hex: &str, height: u32) -> Result<()> {
        let raw_hex = raw_hex.to_owned();
        self.with_db(move |db| {
            let params = regtest_network();
            let height = BlockHeight::from_u32(height);
            let raw = hex::decode(raw_hex).context("invalid transaction hex from Zakura")?;
            let tx = Transaction::read(&raw[..], BranchId::for_height(&params, height))
                .context("parsing transaction from Zakura")?;
            decrypt_and_store_transaction(&params, db, &tx, Some(height))?;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
#[path = "../tests/unit/wallet/treasury_sync_tests.rs"]
mod treasury_sync_tests;

#[cfg(test)]
#[path = "../tests/unit/wallet/tests.rs"]
mod tests;
