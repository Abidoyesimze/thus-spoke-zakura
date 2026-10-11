use std::{future::Future, time::Duration};

use anyhow::{Context, Result, bail};
use futures_util::TryStreamExt;
use tokio::time::{Instant, timeout_at};
use zcash_client_backend::{
    data_api::{
        chain::ChainState,
        scanning::{ScanPriority, ScanRange},
    },
    proto::{
        compact_formats::CompactBlock,
        service::{
            BlockId, BlockRange, ChainSpec, GetAddressUtxosArg, GetSubtreeRootsArg,
            ShieldedProtocol as ProtoShieldedProtocol,
            compact_tx_streamer_client::CompactTxStreamerClient,
        },
    },
};
use zcash_protocol::consensus::BlockHeight;

use crate::rpc::{ChainCheckpoint, NodeRpc};
use crate::wallet::{
    RealWallet, ScanOutcome, ShieldedProtocol, SubtreeRootData, TransparentOutputData,
    TransparentQuery, WALLET_BIRTHDAY_HEIGHT,
};

const NETWORK_TIMEOUT: Duration = Duration::from_secs(30);
const SCAN_BATCH_SIZE: u32 = 100;

pub(crate) async fn find_rewind<F, Fut>(
    wallet: &RealWallet,
    target: &ChainCheckpoint,
    deadline: Instant,
    mut canonical_hash: F,
) -> Result<Option<ChainCheckpoint>>
where
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = Result<String>>,
{
    let Some(scanned) = wallet.max_scanned_checkpoint().await? else {
        return Ok(None);
    };
    anyhow::ensure!(
        target.height >= u64::from(WALLET_BIRTHDAY_HEIGHT - 1),
        "chain is below the wallet birthday floor; waiting for canonical block 1"
    );
    let mut height = u32::try_from(scanned.height.min(target.height))?;
    loop {
        check_deadline(deadline)?;
        let hash =
            within_deadline(deadline, "validating wallet block", canonical_hash(height)).await?;
        if wallet.block_hash(height).await?.as_ref() == Some(&hash) {
            return Ok(
                (u64::from(height) != scanned.height).then_some(ChainCheckpoint {
                    height: u64::from(height),
                    hash,
                }),
            );
        }
        if height < WALLET_BIRTHDAY_HEIGHT {
            return Ok(Some(ChainCheckpoint {
                height: u64::from(height),
                hash,
            }));
        }
        height -= 1;
    }
}

pub async fn sync_wallet(
    wallet: &RealWallet,
    rpc: &NodeRpc,
    target: &ChainCheckpoint,
    deadline: Instant,
) -> Result<()> {
    let mut rewind = find_rewind(wallet, target, deadline, |height| rpc.block_hash(height)).await?;
    if let Some(cursor) = wallet.treasury_cursor().await? {
        let canonical = if target.height >= u64::from(cursor.height) {
            Some(
                within_deadline(
                    deadline,
                    "validating treasury discovery cursor",
                    rpc.block_hash(cursor.height),
                )
                .await?,
            )
        } else {
            None
        };
        if canonical.as_deref() != Some(cursor.block_hash.as_str()) {
            let floor = WALLET_BIRTHDAY_HEIGHT - 1;
            anyhow::ensure!(
                target.height >= u64::from(floor),
                "chain is below the wallet birthday floor"
            );
            rewind = Some(ChainCheckpoint {
                height: u64::from(floor),
                hash: within_deadline(
                    deadline,
                    "validating treasury rewind floor",
                    rpc.block_hash(floor),
                )
                .await?,
            });
        }
    }
    let mut client = within_deadline(
        deadline,
        "connecting to lightwalletd",
        CompactTxStreamerClient::connect(wallet.lightwalletd().to_owned()),
    )
    .await?;
    if let Some(checkpoint) = rewind {
        rewind_wallet(wallet, &mut client, &checkpoint, deadline).await?;
    }
    update_subtree_roots(wallet, &mut client, deadline).await?;
    while sync_pass(wallet, rpc, &mut client, deadline).await? {}
    check_deadline(deadline)
}

async fn rewind_wallet(
    wallet: &RealWallet,
    client: &mut CompactTxStreamerClient<tonic::transport::Channel>,
    checkpoint: &ChainCheckpoint,
    deadline: Instant,
) -> Result<()> {
    let height = u32::try_from(checkpoint.height)?;
    let chain_state = download_chain_state(client, height.into(), deadline).await?;
    anyhow::ensure!(
        chain_state.block_height() == height.into()
            && chain_state.block_hash().to_string() == checkpoint.hash,
        "lightwalletd has not indexed the canonical rewind checkpoint"
    );
    check_deadline(deadline)?;
    wallet.rewind_to_height(chain_state).await?;
    check_deadline(deadline)
}

async fn sync_pass(
    wallet: &RealWallet,
    rpc: &NodeRpc,
    client: &mut CompactTxStreamerClient<tonic::transport::Channel>,
    deadline: Instant,
) -> Result<bool> {
    check_deadline(deadline)?;
    let tip = within_deadline(
        deadline,
        "reading the lightwalletd tip",
        client.get_latest_block(ChainSpec::default()),
    )
    .await?
    .into_inner()
    .height;
    wallet.update_chain_tip(tip).await?;

    for query in wallet.public_transparent_queries().await? {
        refresh_transparent_outputs(wallet, client, query, deadline).await?;
    }

    let mut ranges = wallet.suggested_scan_ranges().await?;
    loop {
        match ranges.first() {
            Some(range) if range.priority() == ScanPriority::Verify => {
                if scan_range(wallet, rpc, client, range.clone(), deadline).await? {
                    ranges = wallet.suggested_scan_ranges().await?;
                } else {
                    break;
                }
            }
            _ => break,
        }
    }

    for range in wallet.suggested_scan_ranges().await? {
        for batch in split_range(range) {
            if scan_range(wallet, rpc, client, batch, deadline).await? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

async fn update_subtree_roots(
    wallet: &RealWallet,
    client: &mut CompactTxStreamerClient<tonic::transport::Channel>,
    deadline: Instant,
) -> Result<()> {
    for (wire, wallet_protocol) in [
        (ProtoShieldedProtocol::Sapling, ShieldedProtocol::Sapling),
        (ProtoShieldedProtocol::Orchard, ShieldedProtocol::Orchard),
        (ProtoShieldedProtocol::Ironwood, ShieldedProtocol::Ironwood),
    ] {
        let mut request = GetSubtreeRootsArg::default();
        request.set_shielded_protocol(wire);
        let response = within_deadline(
            deadline,
            "downloading shielded subtree roots",
            client.get_subtree_roots(request),
        )
        .await?;
        let roots = within_deadline(
            deadline,
            "streaming shielded subtree roots",
            response
                .into_inner()
                .map_ok(|root| SubtreeRootData {
                    completing_height: root.completing_block_height as u32,
                    root_hash: root.root_hash,
                })
                .try_collect::<Vec<_>>(),
        )
        .await?;
        wallet.put_subtree_roots(wallet_protocol, roots).await?;
    }
    Ok(())
}

async fn refresh_transparent_outputs(
    wallet: &RealWallet,
    client: &mut CompactTxStreamerClient<tonic::transport::Channel>,
    query: TransparentQuery,
    deadline: Instant,
) -> Result<()> {
    if query.addresses.is_empty() {
        return Ok(());
    }
    let response = within_deadline(
        deadline,
        "starting transparent output discovery",
        client.get_address_utxos_stream(GetAddressUtxosArg {
            addresses: query.addresses,
            start_height: query.start_height,
            max_entries: 0,
        }),
    )
    .await?;
    let mut stream = response.into_inner();
    let mut outputs = Vec::with_capacity(SCAN_BATCH_SIZE as usize);
    loop {
        let next =
            within_deadline(deadline, "reading transparent outputs", stream.try_next()).await?;
        let Some(reply) = next else {
            break;
        };
        outputs.push(TransparentOutputData {
            txid: reply
                .txid
                .try_into()
                .map_err(|_| anyhow::anyhow!("lightwalletd returned an invalid UTXO txid"))?,
            index: reply
                .index
                .try_into()
                .map_err(|_| anyhow::anyhow!("lightwalletd returned an invalid UTXO index"))?,
            value_zatoshi: reply.value_zat,
            script: reply.script,
            height: reply
                .height
                .try_into()
                .map_err(|_| anyhow::anyhow!("lightwalletd returned an invalid UTXO height"))?,
            account_id: query.account_id,
        });
        if outputs.len() == SCAN_BATCH_SIZE as usize {
            wallet
                .insert_transparent_outputs(std::mem::take(&mut outputs))
                .await?;
        }
    }
    if !outputs.is_empty() {
        wallet.insert_transparent_outputs(outputs).await?;
    }
    Ok(())
}

async fn scan_range(
    wallet: &RealWallet,
    rpc: &NodeRpc,
    client: &mut CompactTxStreamerClient<tonic::transport::Channel>,
    range: ScanRange,
    deadline: Instant,
) -> Result<bool> {
    check_deadline(deadline)?;
    let block_range = range.block_range();
    if block_range.is_empty() {
        return Ok(false);
    }
    let blocks = download_blocks(client, &range, deadline).await?;
    let chain_state = download_chain_state(client, block_range.start - 1, deadline).await?;
    check_deadline(deadline)?;
    let outcome = wallet.scan_batch(range, blocks, chain_state).await?;
    check_deadline(deadline)?;
    match outcome {
        ScanOutcome::Scanned(ranges_changed) => Ok(ranges_changed),
        ScanOutcome::Rewind(height) => {
            let hash = within_deadline(
                deadline,
                "validating continuity rewind",
                rpc.block_hash(height),
            )
            .await?;
            rewind_wallet(
                wallet,
                client,
                &ChainCheckpoint {
                    height: u64::from(height),
                    hash,
                },
                deadline,
            )
            .await?;
            Ok(true)
        }
    }
}

async fn download_blocks(
    client: &mut CompactTxStreamerClient<tonic::transport::Channel>,
    range: &ScanRange,
    deadline: Instant,
) -> Result<Vec<CompactBlock>> {
    let mut start = BlockId::default();
    start.height = range.block_range().start.into();
    let mut end = BlockId::default();
    end.height = (range.block_range().end - 1).into();
    let response = within_deadline(
        deadline,
        "downloading compact blocks",
        client.get_block_range(BlockRange {
            start: Some(start),
            end: Some(end),
            pool_types: vec![],
        }),
    )
    .await?;
    within_deadline(
        deadline,
        "streaming compact blocks",
        response.into_inner().try_collect::<Vec<_>>(),
    )
    .await
}

async fn download_chain_state(
    client: &mut CompactTxStreamerClient<tonic::transport::Channel>,
    height: BlockHeight,
    deadline: Instant,
) -> Result<ChainState> {
    within_deadline(
        deadline,
        "downloading compact-block tree state",
        client.get_tree_state(BlockId {
            height: height.into(),
            hash: vec![],
        }),
    )
    .await?
    .into_inner()
    .to_chain_state()
    .context("lightwalletd returned an invalid tree state")
}

fn split_range(range: ScanRange) -> impl Iterator<Item = ScanRange> {
    (0..).scan(range, |remaining, _| {
        if remaining.is_empty() {
            None
        } else if let Some((current, next)) =
            remaining.split_at(remaining.block_range().start + SCAN_BATCH_SIZE)
        {
            *remaining = next;
            Some(current)
        } else {
            let current = remaining.clone();
            let end = remaining.block_range().end;
            *remaining = ScanRange::from_parts(end..end, remaining.priority());
            Some(current)
        }
    })
}

pub(crate) fn check_deadline(deadline: Instant) -> Result<()> {
    anyhow::ensure!(
        Instant::now() < deadline,
        "wallet reconciliation deadline exceeded"
    );
    Ok(())
}

pub(crate) async fn within_deadline<T, E>(
    deadline: Instant,
    label: &str,
    future: impl Future<Output = Result<T, E>>,
) -> Result<T>
where
    E: Into<anyhow::Error>,
{
    check_deadline(deadline)?;
    match timeout_at(deadline.min(Instant::now() + NETWORK_TIMEOUT), future).await {
        Ok(result) => result.map_err(Into::into).with_context(|| label.to_owned()),
        Err(_) => bail!("{label} exceeded the network or reconciliation deadline"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Store;
    use zcash_primitives::block::BlockHash;

    #[tokio::test]
    async fn same_height_and_lower_tip_reorgs_find_the_last_common_block() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("server.db")).unwrap();
        store.initialize().unwrap();
        let wallet = RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();
        assert_eq!(
            find_rewind(
                &wallet,
                &ChainCheckpoint {
                    height: 0,
                    hash: "00".repeat(32)
                },
                Instant::now() + Duration::from_secs(1),
                |_| async { Ok("00".repeat(32)) },
            )
            .await
            .unwrap(),
            None
        );
        wallet.update_chain_tip(4).await.unwrap();
        let blocks = (2..=4)
            .map(|height| CompactBlock {
                height,
                hash: vec![height as u8; 32],
                prev_hash: vec![(height - 1) as u8; 32],
                chain_metadata: Some(Default::default()),
                ..Default::default()
            })
            .collect();
        wallet
            .scan_batch(
                ScanRange::from_parts(2.into()..5.into(), ScanPriority::Historic),
                blocks,
                ChainState::empty(1.into(), BlockHash([1; 32])),
            )
            .await
            .unwrap();
        assert_eq!(
            wallet.scanned_checkpoint().await.unwrap().unwrap().height,
            4
        );

        for tip in [4, 3] {
            let rewind = find_rewind(
                &wallet,
                &ChainCheckpoint {
                    height: tip,
                    hash: "ff".repeat(32),
                },
                Instant::now() + Duration::from_secs(1),
                |height| async move {
                    Ok(if height <= 2 {
                        BlockHash([height as u8; 32]).to_string()
                    } else {
                        "ff".repeat(32)
                    })
                },
            )
            .await
            .unwrap();
            assert_eq!(
                rewind,
                Some(ChainCheckpoint {
                    height: 2,
                    hash: "02".repeat(32)
                })
            );
        }

        assert!(
            find_rewind(
                &wallet,
                &ChainCheckpoint {
                    height: 0,
                    hash: "00".repeat(32)
                },
                Instant::now() + Duration::from_secs(1),
                |_| async { Ok("00".repeat(32)) },
            )
            .await
            .is_err()
        );
        assert_eq!(
            wallet.scanned_checkpoint().await.unwrap().unwrap().height,
            4
        );
        assert_eq!(
            find_rewind(
                &wallet,
                &ChainCheckpoint {
                    height: 1,
                    hash: "01".repeat(32)
                },
                Instant::now() + Duration::from_secs(1),
                |_| async { Ok("01".repeat(32)) },
            )
            .await
            .unwrap(),
            Some(ChainCheckpoint {
                height: 1,
                hash: "01".repeat(32)
            })
        );

        wallet
            .rewind_to_height(ChainState::empty(2.into(), BlockHash([2; 32])))
            .await
            .unwrap();
        assert_eq!(
            wallet.scanned_checkpoint().await.unwrap().unwrap().height,
            2
        );
        wallet.update_chain_tip(3).await.unwrap();
        wallet
            .scan_batch(
                ScanRange::from_parts(3.into()..4.into(), ScanPriority::Historic),
                vec![CompactBlock {
                    height: 3,
                    hash: vec![255; 32],
                    prev_hash: vec![2; 32],
                    chain_metadata: Some(Default::default()),
                    ..Default::default()
                }],
                ChainState::empty(2.into(), BlockHash([2; 32])),
            )
            .await
            .unwrap();
        assert_eq!(
            wallet.scanned_checkpoint().await.unwrap(),
            Some(ChainCheckpoint {
                height: 3,
                hash: "ff".repeat(32),
            })
        );

        wallet
            .rewind_to_height(ChainState::empty(1.into(), BlockHash([1; 32])))
            .await
            .unwrap();
        assert_eq!(wallet.scanned_checkpoint().await.unwrap(), None);
    }

    #[tokio::test]
    async fn one_attempt_deadline_is_shared_across_network_calls() {
        let deadline = Instant::now() + Duration::from_millis(100);
        within_deadline(deadline, "first call", async {
            Ok::<_, std::io::Error>(())
        })
        .await
        .unwrap();
        let operation = async {
            tokio::time::sleep_until(deadline + Duration::from_millis(50)).await;
            Ok::<_, std::io::Error>(())
        };
        assert!(
            within_deadline(deadline, "second call", operation)
                .await
                .is_err()
        );
        let polled = std::cell::Cell::new(false);
        assert!(
            within_deadline(deadline, "expired call", async {
                polled.set(true);
                Ok::<_, std::io::Error>(())
            })
            .await
            .is_err()
        );
        assert!(
            !polled.get(),
            "expired attempts must not start another call"
        );
    }

    #[test]
    fn scan_ranges_are_split_without_gaps() {
        let range = ScanRange::from_parts(
            BlockHeight::from_u32(5)..BlockHeight::from_u32(256),
            ScanPriority::Historic,
        );
        let ranges = split_range(range)
            .map(|range| {
                (
                    u32::from(range.block_range().start),
                    u32::from(range.block_range().end),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(ranges, [(5, 105), (105, 205), (205, 256)]);
    }
}
