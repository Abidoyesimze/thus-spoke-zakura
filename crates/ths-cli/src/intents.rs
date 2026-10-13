use anyhow::{Context, Result, bail};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions, TryLockError},
    path::PathBuf,
};

pub(crate) struct IntentJournal {
    kind: &'static str,
    path: PathBuf,
    pub(crate) lock: File,
}

impl IntentJournal {
    pub(crate) fn open(instance_dir: &std::path::Path, kind: &'static str) -> Result<Self> {
        let lock = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(instance_dir.join(format!("{kind}-intents.lock")))
            .with_context(|| format!("opening the {kind} intent lock"))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                bail!(
                    "another {kind} command is running for this environment; retry after it finishes"
                )
            }
            Err(TryLockError::Error(error)) => {
                return Err(error).with_context(|| format!("locking {kind} intents"));
            }
        }
        Ok(Self {
            kind,
            path: instance_dir.join(format!("{kind}-intents.json")),
            lock,
        })
    }

    pub(crate) fn read(&self) -> Result<BTreeMap<String, String>> {
        let kind = self.kind;
        match fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("invalid saved {kind} intents")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(error) => Err(error).with_context(|| format!("reading saved {kind} intents")),
        }
    }

    fn write(&self, entries: &BTreeMap<String, String>) -> Result<()> {
        let temporary = self.path.with_extension("json.tmp");
        let file = File::create(&temporary)?;
        serde_json::to_writer(&file, entries)?;
        file.sync_all()?;
        fs::rename(&temporary, &self.path)?;
        File::open(self.path.parent().context("intent directory is missing")?)?.sync_all()?;
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

    /// Forgets each intent only while it still holds the key whose outcome was observed, so a
    /// late result never clears a newer identity.
    pub(crate) fn clear(&self, settled: &[(&str, &str)]) -> Result<()> {
        let mut entries = self.read()?;
        for (intent, key) in settled {
            if entries.get(*intent).map(String::as_str) == Some(*key) {
                entries.remove(*intent);
            }
        }
        self.write(&entries)
    }
}

impl Drop for IntentJournal {
    fn drop(&mut self) {
        // Closing the file releases this flock only after every inherited copy
        // is closed too. A child forked while the guard is alive can hold that
        // copy until exec, so a following faucet command would still see the
        // journal as busy. Unlocking releases every copy of this descriptor.
        if let Err(error) = self.lock.unlock() {
            eprintln!("could not release the {} intent lock: {error}", self.kind);
        }
    }
}
