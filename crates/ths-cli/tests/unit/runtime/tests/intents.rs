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

#[test]
#[cfg(unix)]
fn dropping_faucet_journal_releases_lock_with_a_duplicated_handle() {
    let dir = tempfile::tempdir().unwrap();
    let journal = FaucetJournal::open(dir.path()).unwrap();
    let key = journal.key_for("address:recipient:100").unwrap();
    // A concurrent process spawn can temporarily inherit the same open file
    // description. Keep a duplicate alive to reproduce that lifetime without timing.
    let duplicate = journal.lock.try_clone().unwrap();
    let contender = OpenOptions::new()
        .write(true)
        .open(dir.path().join("faucet-intents.lock"))
        .unwrap();
    assert!(matches!(
        contender.try_lock(),
        Err(TryLockError::WouldBlock)
    ));

    drop(journal);
    let retry = FaucetJournal::open(dir.path()).unwrap();
    assert_eq!(retry.key_for("address:recipient:100").unwrap(), key);
    assert!(matches!(
        contender.try_lock(),
        Err(TryLockError::WouldBlock)
    ));

    drop(duplicate);
    assert!(matches!(
        contender.try_lock(),
        Err(TryLockError::WouldBlock)
    ));
    drop(retry);
    contender.try_lock().unwrap();
}
#[cfg(unix)]
struct InheritedDescriptorChild {
    release: Option<std::os::unix::net::UnixStream>,
    worker: Option<std::thread::JoinHandle<()>>,
}

#[cfg(unix)]
impl InheritedDescriptorChild {
    fn spawn() -> Self {
        use std::io::Read;
        use std::os::fd::AsRawFd;
        use std::os::unix::{net::UnixStream, process::CommandExt};

        let program = ["/usr/bin/true", "/bin/true"]
            .into_iter()
            .find(|path| std::path::Path::new(path).is_file())
            .expect("a program that exits immediately");
        let (mut ready_read, ready_write) = UnixStream::pair().unwrap();
        let (release_read, release_write) = UnixStream::pair().unwrap();
        ready_read
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut command = Command::new(program);
        // SAFETY: the hook runs between fork and exec and only transfers one
        // byte on each barrier socket. Those reads and writes are async-signal-safe,
        // and the sockets stay open because this closure owns them until exec.
        unsafe {
            command.pre_exec(move || {
                transfer_barrier_byte(ready_write.as_raw_fd(), false)?;
                transfer_barrier_byte(release_read.as_raw_fd(), true)?;
                Ok(())
            });
        }
        let worker = std::thread::spawn(move || {
            command.spawn().unwrap().wait().unwrap();
        });
        let child = Self {
            release: Some(release_write),
            worker: Some(worker),
        };
        let mut byte = [0u8; 1];
        ready_read.read_exact(&mut byte).unwrap_or_else(|error| {
            panic!("child did not pause before exec with the inherited descriptor: {error}")
        });
        child
    }

    fn release(mut self) {
        self.unblock();
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }

    fn unblock(&mut self) {
        use std::io::Write;
        if let Some(mut release) = self.release.take() {
            // Closing the socket after a failed write makes the child's read
            // return end-of-file, so it exits instead of staying paused.
            if let Err(error) = release.write_all(&[1]) {
                eprintln!("could not release the inherited-descriptor child: {error}");
            }
        }
    }
}

#[cfg(unix)]
impl Drop for InheritedDescriptorChild {
    fn drop(&mut self) {
        // Unblock a child that is still before exec when an assertion fails,
        // so the worker thread cannot keep the test process waiting.
        self.unblock();
    }
}

/// reads or writes one byte on a barrier socket that stays open until exec.
#[cfg(unix)]
fn transfer_barrier_byte(fd: i32, reading: bool) -> std::io::Result<()> {
    let mut byte = [0u8; 1];
    loop {
        // SAFETY: `fd` is a blocking socket owned by the pre_exec closure.
        // `read` and `write` of one byte do not allocate on the success path.
        let transferred = unsafe {
            if reading {
                inherited_read(fd, byte.as_mut_ptr().cast(), 1)
            } else {
                inherited_write(fd, byte.as_ptr().cast(), 1)
            }
        };
        if transferred == 1 {
            return Ok(());
        }
        if transferred == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "faucet lock barrier closed",
            ));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

#[cfg(unix)]
unsafe extern "C" {
    #[link_name = "read"]
    fn inherited_read(fd: i32, buf: *mut std::ffi::c_void, count: usize) -> isize;
    #[link_name = "write"]
    fn inherited_write(fd: i32, buf: *const std::ffi::c_void, count: usize) -> isize;
}

#[cfg(unix)]
#[test]
fn dropping_the_journal_releases_the_lock_while_a_child_holds_its_descriptor() {
    let dir = tempfile::tempdir().unwrap();
    let journal = FaucetJournal::open(dir.path()).unwrap();
    let child = InheritedDescriptorChild::spawn();
    drop(journal);

    let reopened = FaucetJournal::open(dir.path())
        .unwrap_or_else(|error| panic!("inherited descriptor kept the faucet lock: {error:#}"));
    match FaucetJournal::open(dir.path()) {
        Err(error) => assert!(
            error
                .to_string()
                .contains("another faucet command is running"),
            "{error:#}"
        ),
        Ok(_) => panic!("reopened journal did not keep the faucet lock"),
    }
    drop(reopened);
    child.release();
}
