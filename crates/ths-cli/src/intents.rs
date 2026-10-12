use anyhow::{Context, Result, bail};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions, TryLockError},
    path::PathBuf,
};

pub(crate) struct FaucetJournal {
    path: PathBuf,
    pub(crate) lock: File,
}

impl FaucetJournal {
    pub(crate) fn open(instance_dir: &std::path::Path) -> Result<Self> {
        let lock = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(instance_dir.join("faucet-intents.lock"))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                bail!(
                    "another faucet command is running for this environment; retry after it finishes"
                )
            }
            Err(TryLockError::Error(error)) => {
                return Err(error).context("locking faucet intents");
            }
        }
        Ok(Self {
            path: instance_dir.join("faucet-intents.json"),
            lock,
        })
    }

    fn read(&self) -> Result<BTreeMap<String, String>> {
        match fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("invalid saved faucet intents"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(error) => Err(error).context("reading saved faucet intents"),
        }
    }

    fn write(&self, entries: &BTreeMap<String, String>) -> Result<()> {
        let temporary = self.path.with_extension("json.tmp");
        let file = File::create(&temporary)?;
        serde_json::to_writer(&file, entries)?;
        file.sync_all()?;
        fs::rename(&temporary, &self.path)?;
        File::open(
            self.path
                .parent()
                .context("faucet intent directory is missing")?,
        )?
        .sync_all()?;
        Ok(())
    }

    pub(crate) fn key_for(&self, intent: &str) -> Result<String> {
        let mut entries = self.read()?;
        if let Some(key) = entries.get(intent) {
            return Ok(key.clone());
        }
        let key = uuid::Uuid::new_v4().to_string();
        entries.insert(intent.to_owned(), key.clone());
        self.write(&entries)?;
        Ok(key)
    }

    pub(crate) fn wallet_batch(&self, pool: &str, amount: u64, accounts: &[u8]) -> Result<String> {
        let mut canonical = accounts.to_vec();
        canonical.sort_unstable();
        if canonical.is_empty() || canonical.windows(2).any(|pair| pair[0] == pair[1]) {
            bail!("accounts must contain unique account indices");
        }
        let account_list = canonical
            .iter()
            .map(u8::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let prefix = format!("wallet:{pool}:{amount}:");
        for intent in self.read()?.keys() {
            if let Some((prior_list, _)) = intent
                .strip_prefix(&prefix)
                .and_then(|rest| rest.rsplit_once(':'))
                && prior_list != account_list
                && let Some(overlap) = account_list
                    .split(',')
                    .find(|id| prior_list.split(',').any(|prior| prior == *id))
            {
                bail!(
                    "account {overlap} belongs to an unfinished faucet batch; retry with --accounts {prior_list}"
                );
            }
        }
        Ok(format!("{prefix}{account_list}"))
    }

    pub(crate) fn clear(&self, intents: &[&str]) -> Result<()> {
        let mut entries = self.read()?;
        for intent in intents {
            entries.remove(*intent);
        }
        self.write(&entries)
    }
}

impl Drop for FaucetJournal {
    fn drop(&mut self) {
        // Closing the file releases this flock only after every inherited copy
        // is closed too. A child forked while the guard is alive can hold that
        // copy until exec, so a following faucet command would still see the
        // journal as busy. Unlocking releases every copy of this descriptor.
        if let Err(error) = self.lock.unlock() {
            eprintln!("could not release the faucet intent lock: {error}");
        }
    }
}
