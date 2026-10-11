use super::*;

#[test]
fn faucet_keys_survive_cli_restart_until_the_whole_intent_is_confirmed() {
    let dir = std::env::temp_dir().join(format!("ths-faucet-journal-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&dir).unwrap();
    let first = FaucetJournal::open(&dir).unwrap();
    let account_one = first.key_for("wallet:ironwood:100:1,2:1").unwrap();
    let account_two = first.key_for("wallet:ironwood:100:1,2:2").unwrap();
    drop(first);

    let retry = FaucetJournal::open(&dir).unwrap();
    assert_eq!(
        retry.key_for("wallet:ironwood:100:1,2:1").unwrap(),
        account_one
    );
    assert_eq!(
        retry.key_for("wallet:ironwood:100:1,2:2").unwrap(),
        account_two
    );
    retry
        .clear(&["wallet:ironwood:100:1,2:1", "wallet:ironwood:100:1,2:2"])
        .unwrap();
    drop(retry);

    let next = FaucetJournal::open(&dir).unwrap();
    assert_ne!(
        next.key_for("wallet:ironwood:100:1,2:1").unwrap(),
        account_one
    );
    drop(next);
    fs::remove_dir_all(dir).unwrap();
}

pub(super) fn overlapping_faucet_process_rejects_the_locked_journal() {
    let dir = tempfile::tempdir().unwrap();
    let journal = FaucetJournal::open(dir.path()).unwrap();
    let mut retry = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "runtime::tests::faucet_journal_lock_probe"])
        .env("THS_FAUCET_JOURNAL_LOCK_PROBE", dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let completed_while_locked = loop {
        if retry.try_wait().unwrap().is_some() {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    drop(journal);
    let output = retry.wait_with_output().unwrap();
    let result = String::from_utf8_lossy(&output.stdout);
    assert!(
        completed_while_locked && output.status.success() && result.contains("running 1 test"),
        "overlapping command waited or acquired the lock: {result}"
    );
}

#[test]
fn unfinished_wallet_batch_rejects_an_overlapping_subset() {
    let dir = std::env::temp_dir().join(format!("ths-faucet-batch-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&dir).unwrap();
    let journal = FaucetJournal::open(&dir).unwrap();
    let batch = journal.wallet_batch("ironwood", 100, &[1, 2]).unwrap();
    journal.key_for(&format!("{batch}:1")).unwrap();
    assert!(journal.wallet_batch("ironwood", 100, &[2]).is_err());
    journal.key_for(&format!("{batch}:2")).unwrap();

    assert_eq!(
        journal.wallet_batch("ironwood", 100, &[2, 1]).unwrap(),
        batch
    );
    assert!(journal.wallet_batch("ironwood", 100, &[2]).is_err());
    journal
        .clear(&[&format!("{batch}:1"), &format!("{batch}:2")])
        .unwrap();
    assert!(journal.wallet_batch("ironwood", 100, &[2]).is_ok());
    drop(journal);
    fs::remove_dir_all(dir).unwrap();
}
