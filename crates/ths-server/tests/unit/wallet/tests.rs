use super::*;

fn subtree_end_heights(path: &Path) -> (Option<u32>, Option<u32>, Option<u32>) {
    let db = rusqlite::Connection::open(path.join("wallet.db")).unwrap();
    let height = |table: &str| {
        db.query_row(
            &format!("SELECT subtree_end_height FROM {table} WHERE shard_index = 0"),
            [],
            |row| row.get(0),
        )
        .optional()
        .unwrap()
    };
    (
        height("sapling_tree_shards"),
        height("orchard_tree_shards"),
        height("ironwood_tree_shards"),
    )
}

#[tokio::test]
async fn subtree_roots_reject_malformed_data_and_route_by_protocol() {
    let dir = tempfile::tempdir().unwrap();
    let wallet = RealWallet::open(dir.path(), &hex::encode([7_u8; 64])).unwrap();

    for protocol in [
        ShieldedProtocol::Sapling,
        ShieldedProtocol::Orchard,
        ShieldedProtocol::Ironwood,
    ] {
        wallet
            .put_subtree_roots(
                protocol,
                vec![
                    SubtreeRootData {
                        completing_height: 100,
                        root_hash: vec![0; 32],
                    },
                    SubtreeRootData {
                        completing_height: 101,
                        root_hash: vec![0; 31],
                    },
                ],
            )
            .await
            .unwrap_err();
    }
    assert_eq!(subtree_end_heights(dir.path()), (None, None, None));

    for (protocol, completing_height) in [
        (ShieldedProtocol::Sapling, 100),
        (ShieldedProtocol::Orchard, 101),
        (ShieldedProtocol::Ironwood, 102),
    ] {
        wallet
            .put_subtree_roots(
                protocol,
                vec![SubtreeRootData {
                    completing_height,
                    root_hash: vec![0; 32],
                }],
            )
            .await
            .unwrap();
    }
    assert_eq!(
        subtree_end_heights(dir.path()),
        (Some(100), Some(101), Some(102))
    );
}
