use std::{
    cell::RefCell,
    collections::BTreeMap,
    ffi::OsString,
    fmt::{self, Display},
    fs::{self, File, OpenOptions, TryLockError},
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    str::FromStr,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, AtomicI32, Ordering},
        mpsc::{self, RecvTimeoutError, TryRecvError},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crate::lifecycle::{
    Cancellation, CapturedOutput, CommandFailure, CommandFailureKind, Deadline, HelperSet,
    LifecyclePolicy, OutputMode, run,
};

use anyhow::{Context, Result, anyhow, bail};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

const APP_IMAGE_REPOSITORY: &str = "ghcr.io/zcashlabs/thus-spoke-zakura-app";
const ZAKURA_IMAGE: &str = "zakuracore/zakura:1.6.0";
const LIGHTWALLETD_IMAGE_REPOSITORY: &str = "ghcr.io/zcashlabs/thus-spoke-zakura-lightwalletd";
const INSTANCE_LABEL: &str = "com.zakura.ths.instance";

#[cfg(test)]
#[path = "../tests/unit/docker_lifecycle.rs"]
mod docker_lifecycle_tests;
#[cfg(test)]
#[path = "../tests/unit/runtime_lifecycle.rs"]
mod lifecycle_tests;
mod readiness;
mod recovery;

fn app_image() -> String {
    format!("{APP_IMAGE_REPOSITORY}:{}", env!("CARGO_PKG_VERSION"))
}

fn lightwalletd_image() -> String {
    format!(
        "{LIGHTWALLETD_IMAGE_REPOSITORY}:{}",
        env!("CARGO_PKG_VERSION")
    )
}

#[derive(Clone, Debug)]
pub struct InstanceName(String);

impl Display for InstanceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for InstanceName {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let valid = !value.is_empty()
            && value.len() <= 40
            && value
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !valid || value.starts_with('-') || value.ends_with('-') {
            bail!("instance names use 1-40 lowercase letters, digits, or internal hyphens");
        }
        Ok(Self(value.to_owned()))
    }
}

fn default_regtest() -> String {
    "regtest".to_owned()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Endpoints {
    pub dashboard: String,
    pub rpc: String,
    pub lightwalletd: String,
    pub p2p: String,
    #[serde(default = "default_regtest")]
    pub network: String,
    #[serde(default)]
    pub tls: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Instance {
    name: String,
    version: u32,
    endpoints: Endpoints,
    /// unix seconds; missing for environments created before it was recorded.
    #[serde(default)]
    created_at: Option<u64>,
}

/// the containers every environment has. `init` is one-shot, so it exits once setup is done.
const SERVICES: [&str; 4] = ["app", "zakura", "lightwalletd", "init"];

/// one container's docker state (`running`, `paused`, `exited`, ...), `missing`, or `foreign`
/// when a container this instance doesn't own holds its name, with its unix-second creation and
/// start times.
#[derive(Debug, Serialize)]
struct ContainerStatus {
    service: &'static str,
    state: String,
    created_at: Option<u64>,
    started_at: Option<u64>,
}

#[derive(Debug, Serialize)]
struct EnvironmentStatus {
    #[serde(flatten)]
    instance: Instance,
    /// `running`, `paused` (every service up, some paused), `degraded` (some services up),
    /// `stopped`, `conflict` when another owner holds one of its container names, or `unknown`
    /// when docker can't be read.
    state: &'static str,
    containers: Vec<ContainerStatus>,
}

#[derive(Debug, Deserialize, Serialize)]
struct MineResult {
    blocks: usize,
    hashes: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct FaucetResult {
    address: String,
    amount_zatoshi: u64,
    txid: String,
    block_hash: Option<String>,
    #[serde(default = "confirmed_status")]
    status: String,
}

fn confirmed_status() -> String {
    "confirmed".to_owned()
}

#[derive(Debug, Deserialize, Serialize)]
struct Activity {
    id: String,
    kind: String,
    from_account: Option<u8>,
    /// Null when the recipient is an address outside the development accounts.
    to_account: Option<u8>,
    #[serde(default)]
    to_address: Option<String>,
    source_pool: String,
    destination_pool: String,
    amount_zatoshi: u64,
    txid: String,
    block_hash: Option<String>,
    status: String,
}

impl Activity {
    fn recipient(&self) -> String {
        match (self.to_account, &self.to_address) {
            (Some(id), _) => format!("account {id}"),
            (None, Some(address)) => address.clone(),
            (None, None) => "an unknown recipient".to_owned(),
        }
    }
}

/// A single-payment ZIP-321 URI as resolved by the server.
#[derive(Deserialize)]
struct PaymentUri {
    address: String,
    destination_pool: String,
    amount_zatoshi: Option<u64>,
    memo: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Account {
    id: u8,
    transparent_address: String,
    unified_address: String,
    transparent_zatoshi: u64,
    ironwood_zatoshi: u64,
}

#[derive(Debug, Deserialize)]
struct ServerStatus {
    wallet_sync: WalletSyncStatus,
}

#[derive(Debug, Deserialize)]
struct WalletSyncStatus {
    state: String,
    error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RuntimeStatus {
    node_height: Option<u64>,
    lightwalletd_height: Option<u64>,
    wallet_scanned_height: Option<u64>,
    wallet_sync_state: String,
    settled: bool,
    settlement_blocker: Option<String>,
}

pub struct Runtime {
    root: PathBuf,
}

struct FaucetJournal {
    path: PathBuf,
    lock: File,
}

impl FaucetJournal {
    fn open(instance_dir: &std::path::Path) -> Result<Self> {
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

    fn key_for(&self, intent: &str) -> Result<String> {
        let mut entries = self.read()?;
        if let Some(key) = entries.get(intent) {
            return Ok(key.clone());
        }
        let key = uuid::Uuid::new_v4().to_string();
        entries.insert(intent.to_owned(), key.clone());
        self.write(&entries)?;
        Ok(key)
    }

    fn wallet_batch(&self, pool: &str, amount: u64, accounts: &[u8]) -> Result<String> {
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

    fn clear(&self, intents: &[&str]) -> Result<()> {
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

struct InstanceLock(File);

impl Drop for InstanceLock {
    fn drop(&mut self) {
        if let Err(error) = self.0.unlock() {
            eprintln!("could not release the instance lock: {error}");
        }
    }
}

impl Runtime {
    pub fn discover() -> Result<Self> {
        let dirs = ProjectDirs::from("com", "zakura", "thus-spoke-zakura")
            .ok_or_else(|| anyhow!("could not determine the platform configuration directory"))?;
        Ok(Self {
            root: dirs.config_dir().to_owned(),
        })
    }

    pub fn doctor(&self, json: bool) -> Result<()> {
        let docker = docker_output(["version", "--format", "{{.Server.Version}}"]);
        let result = serde_json::json!({
            "docker": docker.as_ref().ok(),
            "config_dir": self.root,
            "ok": docker.is_ok(),
        });
        if json {
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        if let Ok(version) = docker {
            if !json {
                println!("✓ Docker {version}\n✓ Config: {}", self.root.display());
            }
        } else {
            bail!("Docker is not reachable; start Docker Desktop or the Docker daemon");
        }
        Ok(())
    }

    pub fn build(&self, dev: bool) -> Result<()> {
        self.doctor(false)?;
        build_project_images(dev)?;
        ensure_image(ZAKURA_IMAGE)?;
        println!("Runtime images are ready.");
        Ok(())
    }

    pub fn pull(&self) -> Result<()> {
        self.doctor(false)?;
        for image in [app_image(), lightwalletd_image(), ZAKURA_IMAGE.to_owned()] {
            println!("Pulling {image}…");
            docker(["pull", &image])?;
        }
        println!("Runtime images are ready.");
        Ok(())
    }

    pub fn start(
        &self,
        name: &InstanceName,
        no_open: bool,
        json: bool,
        port_offset: u16,
    ) -> Result<()> {
        host_ports(port_offset)?;
        let shutdown = Shutdown::install()?;
        let context = LifecycleContext::new(LifecyclePolicy::default());
        let prepared = {
            let requested = || shutdown.try_interrupted();
            let docker = LifecycleDocker::production(
                &context,
                None,
                context.policy.startup_docker,
                Cancellation::Observe(&requested),
            );
            (|| {
                startup_docker_reachable(&docker)?;
                for image in [app_image(), lightwalletd_image(), ZAKURA_IMAGE.to_owned()] {
                    startup_require_image(&docker, &image)?;
                }
                Ok(())
            })()
        };
        if let Err(error) = prepared {
            let failures = context.finish_helpers(Deadline::after(context.policy.startup_docker));
            if !failures.is_empty() {
                return Err(anyhow!(
                    "{error:#}; helper cleanup: {}",
                    failures
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("; ")
                ));
            }
            return Err(error);
        }
        self.start_with_policy(
            name,
            no_open,
            json,
            port_offset,
            &DockerHost,
            &shutdown,
            &context,
        )
    }

    pub fn wait(&self, name: &InstanceName, timeout: Duration, json: bool) -> Result<()> {
        let shutdown = Shutdown::install()?;
        let endpoints = self.read_instance(name)?.endpoints;
        let deadline = Instant::now()
            .checked_add(timeout)
            .context("settlement timeout is too large")?;
        let status = wait_until_settled(
            &|remaining| runtime_status(&endpoints, remaining),
            deadline,
            Duration::from_millis(750),
            &shutdown,
            !json,
        )?;
        if json {
            println!("{}", serde_json::to_string_pretty(&status)?);
        } else {
            println!(
                "{name} is settled at node {}, lightwalletd {}, wallet {}.",
                display_height(status.node_height),
                display_height(status.lightwalletd_height),
                display_height(status.wallet_scanned_height)
            );
        }
        Ok(())
    }

    pub fn run(&self, name: &InstanceName, timeout: Duration, command: &[OsString]) -> Result<u8> {
        let shutdown = Shutdown::install()?;
        let mut context = LifecycleContext::new(LifecyclePolicy::default());
        let deadline = Deadline(
            Instant::now()
                .checked_add(timeout)
                .context("startup timeout is too large")?,
        );
        context.overall_deadline = Some(deadline);
        let prepared = {
            let requested = || shutdown.try_interrupted();
            let docker = LifecycleDocker::production(
                &context,
                Some(deadline),
                context.policy.startup_docker,
                Cancellation::Observe(&requested),
            );
            (|| {
                startup_docker_reachable(&docker)?;
                for image in [app_image(), lightwalletd_image(), ZAKURA_IMAGE.to_owned()] {
                    startup_require_image(&docker, &image)?;
                }
                Ok(())
            })()
        };
        if let Err(error) = prepared {
            let failures = context.finish_helpers(Deadline::after(context.policy.cleanup));
            if failures.is_empty() {
                return Err(error);
            }
            return Err(anyhow!(
                "{error:#}; helper cleanup: {}",
                failures
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ")
            ));
        }
        self.run_with_policy(
            name,
            command,
            &std::env::current_dir().context("reading the current directory")?,
            &DockerHost,
            &shutdown,
            &context,
        )
    }

    pub fn status(&self, name: &InstanceName, json: bool) -> Result<()> {
        let endpoints = self
            .read_instance(name)
            .map(|i| i.endpoints)
            .or_else(|_| inspect_endpoints(&prefix(name)))?;
        let running = container_running(&format!("{}-app", prefix(name)))?;
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({"name": name.to_string(), "running": running, "endpoints": endpoints})
                )?
            );
        } else {
            println!("{}", status_text(name, running, &endpoints));
        }
        Ok(())
    }

    pub fn endpoints(&self, name: &InstanceName, json: bool) -> Result<()> {
        let endpoints = self.read_instance(name)?.endpoints;
        if json {
            println!("{}", serde_json::to_string_pretty(&endpoints)?);
        } else {
            println!("{}", endpoint_lines(&endpoints));
        }
        Ok(())
    }

    pub fn open(&self, name: &InstanceName) -> Result<()> {
        open_url(&self.read_instance(name)?.endpoints.dashboard)
    }

    fn instance_client(&self, name: &InstanceName) -> Result<(String, reqwest::blocking::Client)> {
        let app_container = format!("{}-app", prefix(name));
        if !container_running(&app_container)? {
            bail!("environment {name} is not running; start it with `ths --name {name}`");
        }
        let dashboard = self.read_instance(name)?.endpoints.dashboard;
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()?;
        Ok((dashboard, client))
    }

    pub fn mine(&self, name: &InstanceName, blocks: u32, json: bool) -> Result<()> {
        let (dashboard, client) = self.instance_client(name)?;
        let result: MineResult = client
            .post(format!("{dashboard}/api/v1/mine"))
            .json(&serde_json::json!({"blocks": blocks}))
            .send()
            .map_err(anyhow::Error::from)
            .and_then(decode_response)
            .with_context(|| format!("asking environment {name} to mine {blocks} blocks"))?;
        if json {
            println!("{}", serde_json::to_string_pretty(&result)?);
        } else {
            println!("Mined {} blocks on {name}.", result.blocks);
            if let Some(tip) = result.hashes.last() {
                println!("New tip: {tip}");
            }
        }
        Ok(())
    }

    pub fn faucet(
        &self,
        name: &InstanceName,
        address: &str,
        amount_zatoshi: u64,
        json: bool,
    ) -> Result<()> {
        let (dashboard, client) = self.instance_client(name)?;
        let journal = FaucetJournal::open(&self.instance_dir(name))?;
        let intent = format!("address:{address}:{amount_zatoshi}");
        let idempotency_key = journal.key_for(&intent)?;
        let result: FaucetResult = client
            .post(format!("{dashboard}/api/v1/faucet/address"))
            .json(&serde_json::json!({
                "address": address,
                "amount_zatoshi": amount_zatoshi,
                "idempotency_key": idempotency_key,
            }))
            .send()
            .map_err(anyhow::Error::from)
            .and_then(decode_response)
            .with_context(|| format!("asking environment {name} to fund {address}"))?;
        if result.status == "confirmed" && result.block_hash.is_none() {
            bail!("faucet reported confirmation without a block hash; retry the same command");
        }
        if result.status == "confirmed" {
            journal.clear(&[&intent])?;
        }
        if json {
            println!("{}", serde_json::to_string_pretty(&result)?);
        } else {
            println!("Transaction: {}", result.txid);
            if let Some(block_hash) = &result.block_hash {
                println!(
                    "Sent {} ZEC to {} on {name}.",
                    format_zec(result.amount_zatoshi),
                    result.address
                );
                println!("Confirmed in: {block_hash}");
            } else {
                println!(
                    "Payment to {} is pending. Run the same command to check it again.",
                    result.address
                );
            }
        }
        if result.status == "confirmed" {
            Ok(())
        } else {
            bail!("faucet payment is pending; run the same command to check it again")
        }
    }

    pub fn wallet_faucet(
        &self,
        name: &InstanceName,
        accounts: &[u8],
        amount_zatoshi: u64,
        pool: &str,
        json: bool,
    ) -> Result<()> {
        let (dashboard, client) = self.instance_client(name)?;
        self.wallet_faucet_with(
            name,
            &dashboard,
            &client,
            accounts,
            amount_zatoshi,
            pool,
            json,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn wallet_faucet_with(
        &self,
        name: &InstanceName,
        dashboard: &str,
        client: &reqwest::blocking::Client,
        accounts: &[u8],
        amount_zatoshi: u64,
        pool: &str,
        json: bool,
    ) -> Result<()> {
        let journal = FaucetJournal::open(&self.instance_dir(name))?;
        let batch = journal.wallet_batch(pool, amount_zatoshi, accounts)?;
        let intents: Vec<_> = accounts.iter().map(|id| format!("{batch}:{id}")).collect();
        let mut funded = Vec::new();
        let mut pending = Vec::new();
        let mut failures = Vec::new();
        for (&account_id, intent) in accounts.iter().zip(&intents) {
            let idempotency_key = journal.key_for(intent)?;
            let outcome = client
                .post(format!("{dashboard}/api/v1/faucet"))
                .json(&serde_json::json!({
                    "account_id": account_id,
                    "pool": pool,
                    "amount_zatoshi": amount_zatoshi,
                    "idempotency_key": idempotency_key,
                }))
                .send()
                .with_context(|| format!("asking environment {name} to fund account {account_id}"));
            match outcome.and_then(decode_response::<Activity>) {
                Ok(activity) if activity.status == "confirmed" => funded.push(activity),
                Ok(activity) => pending.push(activity),
                Err(error) => failures.push(format!("account {account_id}: {error:#}")),
            }
        }
        if failures.is_empty() && pending.is_empty() {
            journal.clear(&intents.iter().map(String::as_str).collect::<Vec<_>>())?;
        }
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({"funded": funded, "pending": pending, "failed": failures})
                )?
            );
        } else {
            for activity in &funded {
                println!(
                    "Funded {} with {} ZEC ({} pool) on {name}.",
                    activity.recipient(),
                    format_zec(activity.amount_zatoshi),
                    activity.destination_pool
                );
                println!("  Transaction: {}", activity.txid);
            }
            for activity in &pending {
                eprintln!(
                    "Payment to {} is pending ({}). Run the same command to check it again.",
                    activity.recipient(),
                    activity.txid
                );
            }
            for failure in &failures {
                eprintln!("Failed to fund {failure}");
            }
        }
        if failures.is_empty() && pending.is_empty() {
            Ok(())
        } else {
            bail!(
                "{} of {} faucet requests are pending or failed",
                failures.len() + pending.len(),
                accounts.len()
            )
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn wallet_send(
        &self,
        name: &InstanceName,
        from: u8,
        to: u8,
        source_pool: &str,
        destination_pool: &str,
        amount_zatoshi: u64,
        memo: Option<&str>,
        json: bool,
    ) -> Result<()> {
        let (dashboard, client) = self.instance_client(name)?;
        self.submit_send(
            name,
            &dashboard,
            &client,
            from,
            serde_json::json!({
                "from_account": from,
                "to_account": to,
                "source_pool": source_pool,
                "destination_pool": destination_pool,
                "amount_zatoshi": amount_zatoshi,
                "memo": memo,
            }),
            memo,
            json,
        )
    }

    /// Pays a ZIP-321 URI. The server parses it, so the CLI and the dashboard
    /// accept exactly the same URIs.
    pub fn wallet_send_uri(
        &self,
        name: &InstanceName,
        from: u8,
        source_pool: &str,
        uri: &str,
        amount_zatoshi: Option<u64>,
        json: bool,
    ) -> Result<()> {
        let (dashboard, client) = self.instance_client(name)?;
        let response = client
            .post(format!("{dashboard}/api/v1/zip321/parse"))
            .json(&serde_json::json!({ "uri": uri }))
            .send()
            .with_context(|| format!("asking environment {name} to read the payment URI"))?;
        let request: PaymentUri = decode_response(response)?;
        let amount_zatoshi = match (request.amount_zatoshi, amount_zatoshi) {
            (Some(amount), None) | (None, Some(amount)) => amount,
            (Some(_), Some(_)) => bail!("the payment URI already sets an amount; omit --amount"),
            (None, None) => bail!("the payment URI has no amount; pass --amount"),
        };
        self.submit_send(
            name,
            &dashboard,
            &client,
            from,
            serde_json::json!({
                "from_account": from,
                "to_address": request.address,
                "source_pool": source_pool,
                "destination_pool": request.destination_pool,
                "amount_zatoshi": amount_zatoshi,
                "memo": request.memo,
            }),
            request.memo.as_deref(),
            json,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn submit_send(
        &self,
        name: &InstanceName,
        dashboard: &str,
        client: &reqwest::blocking::Client,
        from: u8,
        mut body: serde_json::Value,
        memo: Option<&str>,
        json: bool,
    ) -> Result<()> {
        body["idempotency_key"] =
            serde_json::Value::from(format!("ths-wallet-send-{}", uuid::Uuid::new_v4()));
        let response = client
            .post(format!("{dashboard}/api/v1/send"))
            .json(&body)
            .send()
            .with_context(|| format!("asking environment {name} to send"))?;
        let activity: Activity = decode_response(response)?;
        if json {
            println!("{}", serde_json::to_string_pretty(&activity)?);
        } else {
            println!(
                "Sent {} ZEC from account {} ({} pool) to {} ({} pool) on {name}.",
                format_zec(activity.amount_zatoshi),
                from,
                activity.source_pool,
                activity.recipient(),
                activity.destination_pool
            );
            if let Some(memo) = memo {
                println!("Memo: {memo}");
            }
            println!("Transaction: {}", activity.txid);
            if let Some(block_hash) = &activity.block_hash {
                println!("Confirmed in: {block_hash}");
            }
        }
        Ok(())
    }

    pub fn wallet_balances(&self, name: &InstanceName, json: bool) -> Result<()> {
        let (dashboard, client) = self.instance_client(name)?;
        let accounts = account_balances(&dashboard, &client, name)?;
        if json {
            println!("{}", serde_json::to_string_pretty(&accounts)?);
        } else {
            println!("{}", balance_lines(&accounts));
        }
        Ok(())
    }

    pub fn logs(
        &self,
        name: &InstanceName,
        service: Option<&str>,
        follow: bool,
        tail: Option<u64>,
        head: Option<u64>,
    ) -> Result<()> {
        let service = service.unwrap_or("app");
        let container = format!("{}-{service}", prefix(name));
        if let Some(lines) = head {
            return print_log_head(&container, lines);
        }
        let args = logs_args(&container, follow, tail);
        docker_inherit(&args.iter().map(String::as_str).collect::<Vec<_>>())
    }

    pub fn stop(&self, name: &InstanceName) -> Result<()> {
        let report = self.verified_cleanup(name)?;
        if report.verified() {
            println!("Stopped and deleted {name} and all of its development data.");
            Ok(())
        } else {
            report.into_result()
        }
    }

    pub fn reset(&self, name: &InstanceName, force: bool) -> Result<()> {
        if !force {
            bail!("reset deletes chain, wallet, and seed data; repeat with --force");
        }
        let report = self.verified_cleanup(name)?;
        if report.verified() {
            println!("Deleted {name}; its Docker volumes cannot be recovered.");
            Ok(())
        } else {
            report.into_result()
        }
    }

    fn verified_cleanup(&self, name: &InstanceName) -> Result<CleanupReport> {
        let _lock = self.lock_instance(name)?;
        let policy = LifecyclePolicy::default();
        let context = LifecycleContext::new(policy);
        let deadline = Deadline::after(policy.cleanup);
        let docker = lifecycle_docker(&context, deadline, Cancellation::Ignore);
        self.cleanup_core(name, &docker, false, deadline, &context)
    }

    pub fn list(&self, json: bool) -> Result<()> {
        let environments = self.environments(&DockerCli)?;
        if json {
            println!("{}", serde_json::to_string_pretty(&environments)?);
        } else if environments.is_empty() {
            println!("No environments yet.");
        } else {
            print!("{}", render_list(&environments, now_unix()?));
        }
        Ok(())
    }

    fn environments(&self, docker: &impl DockerResourceCommands) -> Result<Vec<EnvironmentStatus>> {
        let mut environments = Vec::new();
        if self.root.exists() {
            for entry in fs::read_dir(&self.root)? {
                let path = entry?.path().join("instance.json");
                if path.exists() {
                    let instance = serde_json::from_slice::<Instance>(&fs::read(&path)?)?;
                    let name = instance.name.parse()?;
                    let containers = SERVICES
                        .into_iter()
                        .map(|service| container_status(docker, &name, service))
                        .collect::<Result<Vec<_>>>();
                    environments.push(environment_status(instance, containers));
                }
            }
        }
        environments.sort_by(|left, right| left.instance.name.cmp(&right.instance.name));
        Ok(environments)
    }

    fn instance_dir(&self, name: &InstanceName) -> PathBuf {
        self.root.join(name.to_string())
    }

    fn lock_instance(&self, name: &InstanceName) -> Result<InstanceLock> {
        let lock_dir = self.root.join(".locks");
        fs::create_dir_all(&lock_dir)?;
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(lock_dir.join(format!("{name}.lock")))?;
        match file.try_lock() {
            Ok(()) => Ok(InstanceLock(file)),
            Err(TryLockError::WouldBlock) => {
                bail!("environment {name} is already in use by another command")
            }
            Err(TryLockError::Error(error)) => Err(error).context("locking environment"),
        }
    }
    fn write_instance(&self, name: &InstanceName, endpoints: &Endpoints) -> Result<()> {
        let instance = Instance {
            name: name.to_string(),
            version: 1,
            endpoints: endpoints.clone(),
            created_at: Some(now_unix()?),
        };
        fs::write(
            self.instance_dir(name).join("instance.json"),
            serde_json::to_vec_pretty(&instance)?,
        )?;
        Ok(())
    }
    fn read_instance(&self, name: &InstanceName) -> Result<Instance> {
        let path = self.instance_dir(name).join("instance.json");
        serde_json::from_slice(
            &fs::read(&path).with_context(|| format!("instance {name} does not exist"))?,
        )
        .context("invalid instance metadata")
    }

    fn recovery_path(&self, name: &InstanceName) -> PathBuf {
        self.instance_dir(name).join(recovery::RECOVERY_FILE)
    }

    #[cfg(test)]
    fn delete_instance_resources_with(
        &self,
        name: &InstanceName,
        docker: &impl DockerResourceCommands,
    ) -> Result<()> {
        self.cleanup_with(name, docker, false, &LifecyclePolicy::default())?
            .into_result()
    }

    #[cfg(test)]
    fn delete_instance_resources_with_mode(
        &self,
        name: &InstanceName,
        docker: &impl DockerResourceCommands,
        partial: bool,
    ) -> Result<()> {
        self.cleanup_with(name, docker, partial, &LifecyclePolicy::default())?
            .into_result()
    }

    #[cfg(test)]
    fn cleanup_with(
        &self,
        name: &InstanceName,
        docker: &impl DockerResourceCommands,
        partial: bool,
        policy: &LifecyclePolicy,
    ) -> Result<CleanupReport> {
        let deadline = Deadline::after(policy.cleanup);
        let context = LifecycleContext::new(*policy);
        self.cleanup_core(name, docker, partial, deadline, &context)
    }

    fn cleanup_core(
        &self,
        name: &InstanceName,
        docker: &impl DockerResourceCommands,
        partial: bool,
        deadline: Deadline,
        context: &LifecycleContext,
    ) -> Result<CleanupReport> {
        let recovery_path = self.recovery_path(name);
        let loaded = match recovery::RecoveryJournal::load(recovery_path.clone(), name) {
            Ok(journal) => journal,
            Err(error) => {
                let mut failures = vec![format!("{error:#}")];
                failures.extend(
                    context
                        .finish_helpers(deadline)
                        .iter()
                        .map(ToString::to_string),
                );
                return Ok(CleanupReport {
                    outcome: CleanupOutcome::Uncertain,
                    failures,
                    recovery_path: Some(recovery_path),
                    helpers_finished: context.helpers_finished(),
                });
            }
        };
        let mut journal = loaded;
        let kept = journal
            .as_ref()
            .map(|existing| retained_mutation_resources(existing.record(), false))
            .unwrap_or_default();
        let work_deadline = deadline.saturating_sub(context.policy.termination_reserve);
        let prefix = prefix(name);
        let mut failures = Vec::new();
        let mut uncertain = work_deadline.expired();
        if let Some(existing) = &journal
            && !existing.record().unresolved_helpers.is_empty()
        {
            uncertain = true;
            failures.push(format!(
                "previous launcher helpers remain unverified: {}",
                existing.record().unresolved_helpers.join(", ")
            ));
        }
        if work_deadline.expired() {
            failures.push(format!(
                "cleanup deadline for {name} expired before resource removal"
            ));
        }
        let mut targets: Vec<(String, String, String)> = Vec::new();
        let mut absent_containers = Vec::new();
        let mut consider = |kind: &str, target: &str| -> Result<()> {
            if kept.contains(&(resource_kind(kind), target.to_owned())) {
                let present = match resource_absent(docker, kind, target) {
                    Ok(absent) => !absent,
                    Err(error) => {
                        uncertain = true;
                        failures.push(format!(
                            "kept {kind} {target} but its snapshot failed: {error:#}"
                        ));
                        return Ok(());
                    }
                };
                let recovery::MutationOutcome::Uncertain(reason) =
                    recovery::reconcile_unidentified_create(present)
                else {
                    uncertain = true;
                    failures.push(format!("kept {kind} {target} without an uncertain outcome"));
                    return Ok(());
                };
                failures.push(format!("kept {kind} {target}: {reason}"));
                uncertain = true;
                return Ok(());
            }
            if work_deadline.expired() {
                uncertain = true;
                failures.push(format!("stopped before inspecting {kind} {target}"));
                return Ok(());
            }
            if let Some(id) = journal.as_ref().and_then(|existing| {
                existing
                    .record()
                    .mutations
                    .iter()
                    .find(|mutation| {
                        unresolved_allocation(mutation)
                            && mutation.resource.kind == resource_kind(kind)
                            && mutation.resource.name == target
                    })
                    .and_then(|mutation| mutation.resource.identity.clone())
                    .or_else(|| {
                        existing
                            .record()
                            .resources
                            .iter()
                            .find(|resource| {
                                resource.kind == resource_kind(kind) && resource.name == target
                            })
                            .and_then(|resource| resource.identity.clone())
                    })
            }) {
                let absent = match docker.output(&[kind, "inspect", &id]) {
                    Ok(details) if !details.trim().is_empty() => {
                        let recorded = recovery::ResourceRef {
                            kind: resource_kind(kind),
                            name: target.to_owned(),
                            identity: Some(id.clone()),
                        };
                        match inspected_resource_identity(kind, target, name, &details) {
                            Ok(observed) if recovery::identity_matches(&recorded, &observed) => {
                                targets.push((kind.to_owned(), id, target.to_owned()));
                                return Ok(());
                            }
                            Ok(observed) => {
                                uncertain = true;
                                failures.push(format!(
                                    "recorded {kind} {target} ({id}) does not match {observed}; refusing to adopt it"
                                ));
                                false
                            }
                            Err(error) => prove_recorded_absence(
                                docker,
                                kind,
                                target,
                                &id,
                                &format!("ownership inspection failed: {error:#}"),
                                &mut uncertain,
                                &mut failures,
                            ),
                        }
                    }
                    Ok(_) => prove_recorded_absence(
                        docker,
                        kind,
                        target,
                        &id,
                        "returned an empty inspection",
                        &mut uncertain,
                        &mut failures,
                    ),
                    Err(error) => prove_recorded_absence(
                        docker,
                        kind,
                        target,
                        &id,
                        &format!("could not be inspected: {error:#}"),
                        &mut uncertain,
                        &mut failures,
                    ),
                };
                if absent && kind == "container" {
                    absent_containers.push(recovery::ResourceRef {
                        kind: recovery::ResourceKind::Container,
                        name: target.to_owned(),
                        identity: Some(id),
                    });
                }
                return Ok(());
            }
            let result = owned_resource(docker, kind, target, name)
                .with_context(|| format!("{kind} {target}"));
            match result {
                Ok(Some(id)) => targets.push((kind.to_owned(), id, target.to_owned())),
                Ok(None) => {}
                Err(error) if partial => failures.push(format!("{error:#}")),
                Err(error) => return Err(error),
            }
            Ok(())
        };
        let preflight: Result<()> = (|| {
            for service in ["app", "lightwalletd", "zakura", "init"] {
                consider("container", &format!("{prefix}-{service}"))?;
            }
            for suffix in ["chain", "wallet", "lightwalletd", "config"] {
                consider("volume", &format!("{prefix}-{suffix}"))?;
            }
            consider("network", &prefix)?;
            Ok(())
        })();
        let preflight = preflight.and_then(|()| {
            if journal.is_none() && !targets.is_empty() {
                journal = Some(recovery::RecoveryJournal::create(
                    recovery_path.clone(),
                    name,
                )?);
            }
            Ok(())
        });
        if let Err(error) = preflight {
            failures.push(format!("{error:#}"));
            uncertain = true;
        } else {
            for resource in absent_containers {
                if work_deadline.expired() {
                    uncertain = true;
                    failures.push(format!(
                        "cleanup deadline expired before reconciling {}",
                        resource.name
                    ));
                    break;
                }
                if let Some(existing) = &mut journal
                    && let Err(error) = reconcile_absent_container(existing, docker, &resource)
                {
                    uncertain = true;
                    failures.push(format!("reconciling absent {}: {error:#}", resource.name));
                }
            }
            for (kind, id, label) in targets {
                // Ownership preflight includes dependencies, but deletion waits for
                // every referencing create or start to be durably reconciled.
                if kind != "container"
                    && journal.as_ref().is_some_and(|existing| {
                        retained_mutation_resources(existing.record(), true)
                            .contains(&(resource_kind(&kind), label.clone()))
                    })
                {
                    uncertain = true;
                    failures.push(format!(
                        "kept {kind} {label}: unresolved mutation dependencies"
                    ));
                    continue;
                }
                if work_deadline.expired() {
                    uncertain = true;
                    failures.push(format!("cleanup deadline expired before removing {label}"));
                    break;
                }
                if let Some(existing) = &mut journal {
                    let resource = recovery::ResourceRef {
                        kind: resource_kind(&kind),
                        name: label.clone(),
                        identity: Some(id.clone()),
                    };
                    if let Err(error) =
                        existing.begin(resource, Vec::new(), recovery::MutationOperation::Remove)
                    {
                        failures.push(format!(
                            "could not record removal of {label} before mutation: {error:#}"
                        ));
                        uncertain = true;
                        continue;
                    }
                }
                let removal = match kind.as_str() {
                    "container" => docker.run(&["rm", "-f", &id]),
                    "volume" => docker.run(&["volume", "rm", &id]),
                    _ => docker.run(&["network", "rm", &id]),
                };
                let mut reconciled_diagnostic = None;
                let recovering_container = kind == "container"
                    && journal.as_ref().is_some_and(|existing| {
                        existing.record().mutations.iter().any(|mutation| {
                            unresolved_allocation(mutation)
                                && mutation.resource.name == label
                                && mutation.resource.identity.as_deref() == Some(id.as_str())
                        })
                    });
                let absence = if recovering_container {
                    recorded_identity_absent(docker, &kind, &id)
                } else {
                    resource_absent(docker, &kind, &label)
                };
                let outcome = match absence {
                    Ok(true) => {
                        if let Err(error) = &removal {
                            reconciled_diagnostic = Some(format!(
                                "{label}: {error:#}; absence verified after lost removal reply"
                            ));
                        }
                        let unresolved = journal.as_ref().is_some_and(|existing| {
                            recovery::unresolved_create(
                                existing.record(),
                                &resource_kind(&kind),
                                &label,
                            )
                            .is_some()
                        });
                        let outcome = recovery::reconcile_acknowledged_removal(
                            true,
                            unresolved && !recovering_container,
                        );
                        if let recovery::MutationOutcome::Uncertain(reason) = &outcome {
                            uncertain = true;
                            failures.push(format!("{label}: {reason}"));
                        }
                        outcome
                    }
                    Ok(false) => match removal {
                        Ok(()) => {
                            failures.push(format!("{label} is still present after removal"));
                            recovery::MutationOutcome::ReconciledPresent
                        }
                        Err(error) => {
                            uncertain = true;
                            let reason = format!("{label}: {error:#}");
                            failures.push(reason.clone());
                            recovery::MutationOutcome::Uncertain(reason)
                        }
                    },
                    Err(error) => {
                        uncertain = true;
                        let mut reason =
                            format!("{label} removal could not be verified: {error:#}");
                        if let Err(removal_error) = removal {
                            reason.push_str(&format!("; removal failed: {removal_error:#}"));
                        }
                        failures.push(reason.clone());
                        recovery::MutationOutcome::Uncertain(reason)
                    }
                };
                if let Some(existing) = &mut journal {
                    let index = existing.record().mutations.len() - 1;
                    if let Some(diagnostic) = reconciled_diagnostic
                        && let Err(error) = existing.retain_failures(vec![diagnostic], Vec::new())
                    {
                        uncertain = true;
                        failures.push(format!(
                            "recording reconciled removal of {label}: {error:#}"
                        ));
                    }
                    let absent = outcome == recovery::MutationOutcome::ReconciledAbsent;
                    if let Err(error) = existing.finish(index, outcome, Some(id.clone())) {
                        uncertain = true;
                        failures.push(format!("recording removal of {label}: {error:#}"));
                    } else if absent
                        && kind == "container"
                        && let Err(error) = reconcile_absent_container(
                            existing,
                            docker,
                            &recovery::ResourceRef {
                                kind: recovery::ResourceKind::Container,
                                name: label.clone(),
                                identity: Some(id),
                            },
                        )
                    {
                        uncertain = true;
                        failures.push(format!("reconciling absent {label}: {error:#}"));
                    }
                }
            }
        }
        if journal.as_ref().is_some_and(|existing| {
            !retained_mutation_resources(existing.record(), true).is_empty()
        }) {
            uncertain = true;
        }
        for failure in context.finish_helpers(deadline) {
            uncertain = true;
            failures.push(failure.to_string());
        }
        let helpers_finished = context.helpers_finished();
        if !helpers_finished {
            uncertain = true;
            failures.push(format!(
                "unresolved helpers remain: {}",
                context.unresolved_helpers().join(", ")
            ));
        }
        let outcome = if uncertain {
            CleanupOutcome::Uncertain
        } else if failures.is_empty() {
            CleanupOutcome::VerifiedComplete
        } else {
            CleanupOutcome::Incomplete
        };
        if matches!(outcome, CleanupOutcome::VerifiedComplete) && helpers_finished {
            let dir = self.instance_dir(name);
            if dir.exists()
                && let Err(error) = fs::remove_dir_all(&dir)
            {
                failures.push(format!("removing metadata {}: {error}", dir.display()));
                if let Some(existing) = &mut journal
                    && let Err(error) =
                        existing.retain_failures(failures.clone(), context.unresolved_helpers())
                {
                    failures.push(format!("retaining cleanup recovery: {error:#}"));
                }
                return Ok(CleanupReport {
                    outcome: CleanupOutcome::Incomplete,
                    failures,
                    recovery_path: Some(recovery_path),
                    helpers_finished,
                });
            }
            return Ok(CleanupReport {
                outcome: CleanupOutcome::VerifiedComplete,
                failures,
                recovery_path: None,
                helpers_finished,
            });
        }
        if let Some(existing) = &mut journal
            && let Err(error) =
                existing.retain_failures(failures.clone(), context.unresolved_helpers())
        {
            failures.push(format!("retaining cleanup recovery: {error:#}"));
        }
        Ok(CleanupReport {
            outcome,
            failures,
            recovery_path: Some(recovery_path),
            helpers_finished,
        })
    }
}

fn decode_response<T: serde::de::DeserializeOwned>(
    response: reqwest::blocking::Response,
) -> Result<T> {
    let status = response.status();
    if !status.is_success() {
        let detail = response
            .text()
            .unwrap_or_else(|_| "response body was unreadable".to_owned());
        bail!("rejected ({status}): {detail}");
    }
    response.json().context("decoding response")
}

fn format_zec(zatoshi: u64) -> String {
    let whole = zatoshi / 100_000_000;
    let fraction = zatoshi % 100_000_000;
    if fraction == 0 {
        whole.to_string()
    } else {
        format!("{whole}.{fraction:08}")
            .trim_end_matches('0')
            .to_owned()
    }
}

#[derive(Debug)]
struct HostPorts {
    dashboard: u16,
    rpc: u16,
    p2p: u16,
    lightwalletd: u16,
}

fn host_ports(offset: u16) -> Result<HostPorts> {
    if !offset.is_multiple_of(10) {
        bail!("--port-offset must be a multiple of 10 (got {offset})");
    }
    Ok(HostPorts {
        dashboard: 32805u16
            .checked_add(offset)
            .with_context(|| format!("--port-offset {offset} overflows the dashboard port"))?,
        rpc: 18232u16
            .checked_add(offset)
            .with_context(|| format!("--port-offset {offset} overflows the Zakura RPC port"))?,
        p2p: 18233u16
            .checked_add(offset)
            .with_context(|| format!("--port-offset {offset} overflows the P2P port"))?,
        lightwalletd: 9067u16
            .checked_add(offset)
            .with_context(|| format!("--port-offset {offset} overflows the lightwalletd port"))?,
    })
}

fn loopback_publish(host: u16, container: u16) -> String {
    format!("127.0.0.1:{host}:{container}")
}

fn require_free_loopback(port: u16) -> Result<()> {
    match std::net::TcpListener::bind(("127.0.0.1", port)) {
        Ok(listener) => {
            drop(listener);
            Ok(())
        }
        Err(_) => bail!("port {port} is already in use on 127.0.0.1"),
    }
}

fn prefix(name: &InstanceName) -> String {
    format!("ths-{name}")
}
fn label(name: &InstanceName) -> String {
    format!("{INSTANCE_LABEL}={name}")
}

trait DockerResourceCommands {
    fn output(&self, args: &[&str]) -> Result<String>;
    fn run(&self, args: &[&str]) -> Result<()>;
}

struct DockerCli;

impl DockerResourceCommands for DockerCli {
    fn output(&self, args: &[&str]) -> Result<String> {
        docker_output_args(args)
    }

    fn run(&self, args: &[&str]) -> Result<()> {
        docker_inherit(args)
    }
}

struct LifecycleContext {
    policy: LifecyclePolicy,
    helpers: RefCell<HelperSet>,
    overall_deadline: Option<Deadline>,
}

impl LifecycleContext {
    fn new(policy: LifecyclePolicy) -> Self {
        Self {
            policy,
            helpers: RefCell::new(HelperSet::new()),
            overall_deadline: None,
        }
    }

    fn startup_deadline(&self, duration: Duration) -> Deadline {
        self.overall_deadline.map_or_else(
            || Deadline::after(duration),
            |deadline| deadline.clipped(duration),
        )
    }

    fn finish_helpers(&self, deadline: Deadline) -> Vec<CommandFailure> {
        self.helpers.borrow_mut().finish(deadline, self.policy.poll)
    }

    fn helpers_finished(&self) -> bool {
        self.helpers.borrow().is_empty()
    }

    fn unresolved_helpers(&self) -> Vec<String> {
        self.helpers.borrow().unresolved_context()
    }
}

struct LifecycleDocker<'a> {
    context: &'a LifecycleContext,
    enclosing_deadline: Option<Deadline>,
    command_cap: Duration,
    cancellation: Cancellation<'a>,
    program: String,
    prefix_args: Vec<String>,
    env: Vec<(String, String)>,
}

impl<'a> LifecycleDocker<'a> {
    fn production(
        context: &'a LifecycleContext,
        enclosing_deadline: Option<Deadline>,
        command_cap: Duration,
        cancellation: Cancellation<'a>,
    ) -> Self {
        Self {
            context,
            enclosing_deadline,
            command_cap,
            cancellation,
            program: "docker".to_owned(),
            prefix_args: Vec::new(),
            env: Vec::new(),
        }
    }

    fn scoped(&self, enclosing_deadline: Deadline, command_cap: Duration) -> LifecycleDocker<'a> {
        LifecycleDocker {
            context: self.context,
            enclosing_deadline: Some(enclosing_deadline),
            command_cap,
            cancellation: self.cancellation,
            program: self.program.clone(),
            prefix_args: self.prefix_args.clone(),
            env: self.env.clone(),
        }
    }

    fn operation_deadline(&self) -> Deadline {
        self.enclosing_deadline
            .map(|deadline| deadline.clipped(self.command_cap))
            .unwrap_or_else(|| Deadline::after(self.command_cap))
    }

    fn execute(&self, args: &[&str], mode: OutputMode) -> Result<CapturedOutput, CommandFailure> {
        let mut command = if self.program == "docker" {
            docker_cli()
        } else {
            Command::new(&self.program)
        };
        command.args(&self.prefix_args);
        command.args(args);
        for (key, value) in &self.env {
            command.env(key, value);
        }
        let mut helpers = self.context.helpers.borrow_mut();
        run(
            &mut command,
            self.operation_deadline(),
            self.cancellation,
            mode,
            &self.context.policy,
            &mut helpers,
        )
    }
}

impl DockerResourceCommands for LifecycleDocker<'_> {
    fn output(&self, args: &[&str]) -> Result<String> {
        let captured = self
            .execute(args, OutputMode::Capture)
            .map_err(|error| anyhow!("{error}"))?;
        if captured.stdout_truncated || captured.stderr_truncated {
            bail!("docker {} returned truncated output", args.join(" "));
        }
        let mut text = String::from_utf8(captured.stdout).context("docker output was not utf-8")?;
        if args.first().copied() == Some("logs") {
            text.push_str(&String::from_utf8_lossy(&captured.stderr));
        }
        Ok(text.trim().to_owned())
    }

    fn run(&self, args: &[&str]) -> Result<()> {
        self.execute(args, OutputMode::Inherit)?;
        Ok(())
    }
}

#[derive(Debug)]
enum CleanupOutcome {
    VerifiedComplete,
    Incomplete,
    Uncertain,
}

#[derive(Debug)]
struct CleanupReport {
    outcome: CleanupOutcome,
    failures: Vec<String>,
    recovery_path: Option<PathBuf>,
    helpers_finished: bool,
}

impl CleanupReport {
    fn verified(&self) -> bool {
        matches!(self.outcome, CleanupOutcome::VerifiedComplete) && self.helpers_finished
    }

    fn into_result(self) -> Result<()> {
        if self.verified() {
            Ok(())
        } else if self.failures.is_empty() {
            bail!("cleanup was not verified")
        } else {
            let location = self
                .recovery_path
                .as_ref()
                .map(|path| format!("; recovery remains at {}", path.display()))
                .unwrap_or_default();
            bail!("{}{location}", self.failures.join("; "))
        }
    }
}

struct AllocatedInstance {
    endpoints: Endpoints,
    resources: Vec<recovery::ResourceRef>,
    app_container: WatchedContainer,
    zakura_container_id: String,
}

fn resource_kind(kind: &str) -> recovery::ResourceKind {
    match kind {
        "container" => recovery::ResourceKind::Container,
        "volume" => recovery::ResourceKind::Volume,
        "network" => recovery::ResourceKind::Network,
        _ => unreachable!("resource kind is fixed by the caller"),
    }
}

fn unresolved_allocation(mutation: &recovery::MutationRecord) -> bool {
    matches!(
        mutation.operation,
        recovery::MutationOperation::Create | recovery::MutationOperation::Start
    ) && matches!(
        mutation.outcome,
        recovery::MutationOutcome::Pending | recovery::MutationOutcome::Uncertain(_)
    )
}

fn retained_mutation_resources(
    record: &recovery::RecoveryRecord,
    include_known_containers: bool,
) -> std::collections::HashSet<(recovery::ResourceKind, String)> {
    let mut kept = std::collections::HashSet::new();
    for mutation in &record.mutations {
        let resource = &mutation.resource;
        let known_container = resource.kind == recovery::ResourceKind::Container
            && resource
                .identity
                .as_deref()
                .is_some_and(|id| !id.is_empty())
            && !record.resources.iter().any(|other| {
                other.kind == resource.kind
                    && other.name == resource.name
                    && other.identity.is_some()
                    && other.identity != resource.identity
            })
            && !record.mutations.iter().any(|other| {
                unresolved_allocation(other)
                    && other.resource.kind == resource.kind
                    && other.resource.name == resource.name
                    && other.resource.identity != resource.identity
            });
        if unresolved_allocation(mutation) && (include_known_containers || !known_container) {
            kept.insert((resource.kind.clone(), resource.name.clone()));
            for dependency in &mutation.dependencies {
                kept.insert((dependency.kind.clone(), dependency.name.clone()));
            }
        }
    }
    kept
}

fn reconcile_absent_container(
    journal: &mut recovery::RecoveryJournal,
    docker: &impl DockerResourceCommands,
    resource: &recovery::ResourceRef,
) -> Result<()> {
    if !journal
        .record()
        .mutations
        .iter()
        .any(|mutation| unresolved_allocation(mutation) && mutation.resource == *resource)
    {
        return Ok(());
    }
    // Do not release dependencies underneath a same-name replacement.
    if !resource_absent(docker, "container", &resource.name)? {
        bail!(
            "same-name container {} remains; refusing to release allocation dependencies",
            resource.name
        );
    }
    journal.reconcile_absent_allocations(resource)
}

fn inspected_resource_identity(
    kind: &str,
    target: &str,
    name: &InstanceName,
    details: &str,
) -> Result<String> {
    let resources: serde_json::Value = serde_json::from_str(details)
        .with_context(|| format!("decoding Docker {kind} {target}"))?;
    let items = resources
        .as_array()
        .with_context(|| format!("Docker returned invalid {kind} {target}"))?;
    if items.len() != 1 {
        bail!(
            "Docker returned {} {kind} resources named {target}",
            items.len()
        );
    }
    let resource = &items[0];
    let labels = if kind == "container" {
        &resource["Config"]["Labels"]
    } else {
        &resource["Labels"]
    };
    if labels[INSTANCE_LABEL].as_str() != Some(name.0.as_str()) {
        bail!(
            "Docker {kind} {target} is not owned by ths instance {name}; refusing to reuse or delete it"
        );
    }
    // Containers and networks have stable IDs; Docker identifies volumes by name.
    let id_field = if kind == "volume" { "Name" } else { "Id" };
    let id = resource[id_field]
        .as_str()
        .filter(|id| !id.is_empty())
        .with_context(|| format!("Docker {kind} {target} has no {id_field}"))?;
    Ok(id.to_owned())
}

/// A failed or empty inspect is not absence. A completed identity listing that
/// does not contain the recorded id is.
fn prove_recorded_absence(
    docker: &impl DockerResourceCommands,
    kind: &str,
    target: &str,
    id: &str,
    reason: &str,
    uncertain: &mut bool,
    failures: &mut Vec<String>,
) -> bool {
    match recorded_identity_absent(docker, kind, id) {
        Ok(true) => true,
        Ok(false) => {
            *uncertain = true;
            failures.push(format!("recorded {kind} {target} ({id}) {reason}"));
            false
        }
        Err(error) => {
            *uncertain = true;
            failures.push(format!(
                "recorded {kind} {target} ({id}) {reason}; identity listing failed: {error:#}"
            ));
            false
        }
    }
}

fn recorded_identity_absent(
    docker: &impl DockerResourceCommands,
    kind: &str,
    identity: &str,
) -> Result<bool> {
    let listing = match kind {
        "container" => {
            docker.output(&["container", "ls", "-a", "--no-trunc", "--format", "{{.ID}}"])?
        }
        "network" => docker.output(&["network", "ls", "--no-trunc", "--format", "{{.ID}}"])?,
        "volume" => docker.output(&["volume", "ls", "--format", "{{.Name}}"])?,
        _ => unreachable!("resource kind is fixed by the caller"),
    };
    Ok(!listing
        .lines()
        .any(|candidate| candidate.trim() == identity))
}

fn resource_absent(docker: &impl DockerResourceCommands, kind: &str, name: &str) -> Result<bool> {
    let names = match kind {
        "container" => docker.output(&["container", "ls", "-a", "--format", "{{.Names}}"])?,
        "volume" => docker.output(&["volume", "ls", "--format", "{{.Name}}"])?,
        "network" => docker.output(&["network", "ls", "--format", "{{.Name}}"])?,
        _ => unreachable!("resource kind is fixed by the caller"),
    };
    Ok(!names.lines().any(|candidate| candidate.trim() == name))
}

fn owned_resource(
    docker: &impl DockerResourceCommands,
    kind: &str,
    target: &str,
    name: &InstanceName,
) -> Result<Option<String>> {
    let names = match kind {
        "container" => docker.output(&["container", "ls", "-a", "--format", "{{.Names}}"])?,
        "volume" => docker.output(&["volume", "ls", "--format", "{{.Name}}"])?,
        "network" => docker.output(&["network", "ls", "--format", "{{.Name}}"])?,
        _ => unreachable!("resource kind is fixed by the caller"),
    };
    let matches = names
        .lines()
        .filter(|candidate| candidate.trim() == target)
        .count();
    if matches == 0 {
        return Ok(None);
    }
    if matches > 1 {
        bail!(
            "Docker has multiple {kind} resources named {target}; refusing to reuse or delete them"
        );
    }
    let details = docker.output(&[kind, "inspect", target])?;
    inspected_resource_identity(kind, target, name, &details).map(Some)
}

fn mutation_failure(
    journal: &mut recovery::RecoveryJournal,
    index: usize,
    error: anyhow::Error,
    identity: Option<String>,
) -> anyhow::Error {
    match journal.finish(
        index,
        recovery::MutationOutcome::Uncertain(format!("{error:#}")),
        identity,
    ) {
        Ok(()) => error,
        Err(record_error) => anyhow!("{error:#}; recording mutation uncertainty: {record_error:#}"),
    }
}

fn ensure_network_with(
    prefix: &str,
    name: &InstanceName,
    docker: &impl DockerResourceCommands,
    journal: &mut recovery::RecoveryJournal,
    deadline: Deadline,
) -> Result<recovery::ResourceRef> {
    bounded_ensure(deadline, prefix, || {
        if let Some(id) = owned_resource(docker, "network", prefix, name)? {
            return Ok(recovery::ResourceRef {
                kind: recovery::ResourceKind::Network,
                name: prefix.to_owned(),
                identity: Some(id),
            });
        }
        let resource = recovery::ResourceRef {
            kind: recovery::ResourceKind::Network,
            name: prefix.to_owned(),
            identity: None,
        };
        let index = journal.begin(
            resource.clone(),
            Vec::new(),
            recovery::MutationOperation::Create,
        )?;
        if let Err(error) = docker.run(&["network", "create", "--label", &label(name), prefix]) {
            return Err(mutation_failure(journal, index, error, None));
        }
        match owned_resource(docker, "network", prefix, name)? {
            Some(id) => {
                journal.finish(
                    index,
                    recovery::MutationOutcome::Acknowledged,
                    Some(id.clone()),
                )?;
                Ok(recovery::ResourceRef {
                    identity: Some(id),
                    ..resource
                })
            }
            None => Err(mutation_failure(
                journal,
                index,
                anyhow!("Docker did not create network {prefix}"),
                None,
            )),
        }
    })
}
fn ensure_volume_with(
    volume: &str,
    name: &InstanceName,
    docker: &impl DockerResourceCommands,
    journal: &mut recovery::RecoveryJournal,
    deadline: Deadline,
) -> Result<recovery::ResourceRef> {
    bounded_ensure(deadline, volume, || {
        if let Some(id) = owned_resource(docker, "volume", volume, name)? {
            return Ok(recovery::ResourceRef {
                kind: recovery::ResourceKind::Volume,
                name: volume.to_owned(),
                identity: Some(id),
            });
        }
        let resource = recovery::ResourceRef {
            kind: recovery::ResourceKind::Volume,
            name: volume.to_owned(),
            identity: None,
        };
        let index = journal.begin(
            resource.clone(),
            Vec::new(),
            recovery::MutationOperation::Create,
        )?;
        if let Err(error) = docker.run(&["volume", "create", "--label", &label(name), volume]) {
            return Err(mutation_failure(journal, index, error, None));
        }
        match owned_resource(docker, "volume", volume, name)? {
            Some(id) => {
                journal.finish(
                    index,
                    recovery::MutationOutcome::Acknowledged,
                    Some(id.clone()),
                )?;
                Ok(recovery::ResourceRef {
                    identity: Some(id),
                    ..resource
                })
            }
            None => Err(mutation_failure(
                journal,
                index,
                anyhow!("Docker did not create volume {volume}"),
                None,
            )),
        }
    })
}

fn bounded_ensure<T>(
    deadline: Deadline,
    resource: &str,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    if deadline.expired() {
        bail!("timed out before creating {resource}");
    }
    operation()
}
fn volume_dependency(
    journal: &recovery::RecoveryJournal,
    target: &str,
) -> Result<recovery::ResourceRef> {
    journal
        .record()
        .resources
        .iter()
        .find(|resource| resource.kind == recovery::ResourceKind::Volume && resource.name == target)
        .cloned()
        .with_context(|| format!("missing allocated dependency {target}"))
}

fn ensure_zakura(
    prefix: &str,
    name: &InstanceName,
    ports: &HostPorts,
    docker: &impl DockerResourceCommands,
    journal: &mut recovery::RecoveryJournal,
    deadline: Deadline,
    mut dependencies: Vec<recovery::ResourceRef>,
) -> Result<String> {
    for suffix in ["chain", "config"] {
        dependencies.push(volume_dependency(journal, &format!("{prefix}-{suffix}"))?);
    }

    let rpc_bind = loopback_publish(ports.rpc, 18232);
    let p2p_bind = loopback_publish(ports.p2p, 18233);
    create_container(
        &format!("{prefix}-zakura"),
        name,
        docker,
        journal,
        deadline,
        dependencies,
        &[
            "create",
            "--name",
            &format!("{prefix}-zakura"),
            "--network",
            prefix,
            "--network-alias",
            "zakura",
            "--label",
            &label(name),
            "-p",
            &rpc_bind,
            "-p",
            &p2p_bind,
            "-v",
            &format!("{prefix}-chain:/data"),
            "-v",
            &format!("{prefix}-config:/config:ro"),
            "-e",
            "CONFIG_FILE_PATH=/config/zakurad.toml",
            ZAKURA_IMAGE,
            "zakurad",
            "start",
        ],
    )
}
fn ensure_lightwalletd(
    prefix: &str,
    name: &InstanceName,
    ports: &HostPorts,
    docker: &impl DockerResourceCommands,
    journal: &mut recovery::RecoveryJournal,
    deadline: Deadline,
    mut dependencies: Vec<recovery::ResourceRef>,
) -> Result<String> {
    dependencies.push(volume_dependency(
        journal,
        &format!("{prefix}-lightwalletd"),
    )?);

    let image = lightwalletd_image();
    let lightwalletd_bind = loopback_publish(ports.lightwalletd, 9067);
    let target = format!("{prefix}-lightwalletd");
    create_container(
        &target,
        name,
        docker,
        journal,
        deadline,
        dependencies,
        &[
            "create",
            "--name",
            &target,
            "--network",
            prefix,
            "--network-alias",
            "lightwalletd",
            "--label",
            &label(name),
            "--user",
            "0:0",
            "-p",
            &lightwalletd_bind,
            "-v",
            &format!("{prefix}-lightwalletd:/var/lib/lightwalletd"),
            &image,
            "--no-tls-very-insecure",
            "--grpc-bind-addr",
            "0.0.0.0:9067",
            "--rpchost",
            "zakura",
            "--rpcport",
            "18232",
            "--rpcuser",
            "unused",
            "--rpcpassword",
            "unused",
            "--data-dir",
            "/var/lib/lightwalletd",
            "--log-file",
            "/dev/stdout",
        ],
    )
}
fn ensure_app(
    prefix: &str,
    name: &InstanceName,
    ports: &HostPorts,
    docker: &impl DockerResourceCommands,
    journal: &mut recovery::RecoveryJournal,
    deadline: Deadline,
    mut dependencies: Vec<recovery::ResourceRef>,
) -> Result<WatchedContainer> {
    dependencies.push(volume_dependency(journal, &format!("{prefix}-wallet"))?);

    let public_rpc = format!("http://127.0.0.1:{}", ports.rpc);
    let public_lightwalletd = format!("http://127.0.0.1:{}", ports.lightwalletd);
    let public_p2p = format!("127.0.0.1:{}", ports.p2p);
    let dashboard_bind = loopback_publish(ports.dashboard, 8080);
    let image = app_image();
    let target = format!("{prefix}-app");
    let instance = format!("THS_INSTANCE={name}");
    let public_rpc_env = format!("THS_PUBLIC_ZAKURA_RPC={public_rpc}");
    let public_lightwalletd_env = format!("THS_PUBLIC_LIGHTWALLETD={public_lightwalletd}");
    let public_p2p_env = format!("THS_PUBLIC_P2P={public_p2p}");
    let id = create_container(
        &target,
        name,
        docker,
        journal,
        deadline,
        dependencies,
        &[
            "create",
            "--name",
            &target,
            "--network",
            prefix,
            "--label",
            &label(name),
            "-p",
            &dashboard_bind,
            "-e",
            "THS_LISTEN=0.0.0.0:8080",
            "-e",
            "THS_ZAKURA_RPC=http://zakura:18232",
            "-e",
            "THS_LIGHTWALLETD=http://lightwalletd:9067",
            "-e",
            &instance,
            "-e",
            &public_rpc_env,
            "-e",
            &public_lightwalletd_env,
            "-e",
            &public_p2p_env,
            "-v",
            &format!("{prefix}-wallet:/data"),
            &image,
            "serve",
            "--data-dir",
            "/data",
        ],
    )?;
    if id.is_empty() {
        bail!("Docker returned no ID for app container {target}");
    }
    // Read the replay cursor by allocated ID so a replacement cannot become the watched app.
    let created = docker.output(&["container", "inspect", "--format", "{{.Created}}", &id])?;
    if created.is_empty() {
        bail!("Docker returned no creation time for app container {id}");
    }
    Ok(WatchedContainer { id, created })
}

fn create_container(
    target: &str,
    name: &InstanceName,
    docker: &impl DockerResourceCommands,
    journal: &mut recovery::RecoveryJournal,
    deadline: Deadline,
    dependencies: Vec<recovery::ResourceRef>,
    args: &[&str],
) -> Result<String> {
    if deadline.expired() {
        bail!("timed out before creating {target}");
    }
    if let Some(id) = owned_resource(docker, "container", target, name)? {
        return Ok(id);
    }
    let index = journal.begin(
        recovery::ResourceRef {
            kind: recovery::ResourceKind::Container,
            name: target.to_owned(),
            identity: None,
        },
        dependencies,
        recovery::MutationOperation::Create,
    )?;
    let created = docker.output(args);
    let id = match created {
        Ok(id) => id,
        Err(error) => {
            return Err(mutation_failure(journal, index, error, None));
        }
    };
    if let Err(error) = require_container_identity(docker, &id, name) {
        return Err(mutation_failure(journal, index, error, Some(id)));
    }
    journal.finish(
        index,
        recovery::MutationOutcome::Acknowledged,
        Some(id.clone()),
    )?;
    Ok(id)
}

fn require_container_identity(
    docker: &impl DockerResourceCommands,
    id: &str,
    name: &InstanceName,
) -> Result<()> {
    let details = docker.output(&["container", "inspect", id])?;
    let resources: serde_json::Value = serde_json::from_str(&details)
        .with_context(|| format!("decoding Docker container {id}"))?;
    let items = resources
        .as_array()
        .with_context(|| format!("Docker returned invalid container {id}"))?;
    if items.len() != 1 {
        bail!(
            "Docker returned {} container resources for {id}; refusing to adopt a replacement",
            items.len()
        );
    }
    let resource = &items[0];
    if resource["Id"].as_str() != Some(id) {
        bail!("Docker container {id} changed identity; refusing to adopt a replacement");
    }
    if resource["Config"]["Labels"][INSTANCE_LABEL].as_str() != Some(name.0.as_str()) {
        bail!("Docker container {id} is not owned by ths instance {name}");
    }
    Ok(())
}

fn start_container(
    id: &str,
    name: &str,
    docker: &impl DockerResourceCommands,
    journal: &mut recovery::RecoveryJournal,
    deadline: Deadline,
) -> Result<()> {
    if deadline.expired() {
        bail!("timed out before starting {name}");
    }
    let dependencies = journal
        .record()
        .mutations
        .iter()
        .rev()
        .find(|mutation| {
            mutation.operation == recovery::MutationOperation::Create
                && mutation.resource.name == name
        })
        .with_context(|| format!("missing creation record for {name}"))?
        .dependencies
        .clone();
    let index = journal.begin(
        recovery::ResourceRef {
            kind: recovery::ResourceKind::Container,
            name: name.to_owned(),
            identity: Some(id.to_owned()),
        },
        dependencies,
        recovery::MutationOperation::Start,
    )?;
    if let Err(error) = docker.run(&["start", id]) {
        return Err(mutation_failure(journal, index, error, Some(id.to_owned())));
    }
    journal.finish(
        index,
        recovery::MutationOutcome::Acknowledged,
        Some(id.to_owned()),
    )?;
    Ok(())
}

fn endpoints_for(ports: &HostPorts) -> Endpoints {
    Endpoints {
        dashboard: format!("http://127.0.0.1:{}", ports.dashboard),
        rpc: format!("http://127.0.0.1:{}", ports.rpc),
        lightwalletd: format!("http://127.0.0.1:{}", ports.lightwalletd),
        p2p: format!("127.0.0.1:{}", ports.p2p),
        network: default_regtest(),
        tls: false,
    }
}

fn inspect_endpoints(prefix: &str) -> Result<Endpoints> {
    Ok(Endpoints {
        dashboard: format!(
            "http://127.0.0.1:{}",
            published_port(&format!("{prefix}-app"), "8080/tcp")?
        ),
        rpc: format!(
            "http://127.0.0.1:{}",
            published_port(&format!("{prefix}-zakura"), "18232/tcp")?
        ),
        lightwalletd: format!(
            "http://127.0.0.1:{}",
            published_port(&format!("{prefix}-lightwalletd"), "9067/tcp")?
        ),
        p2p: format!(
            "127.0.0.1:{}",
            published_port(&format!("{prefix}-zakura"), "18233/tcp")?
        ),
        network: default_regtest(),
        tls: false,
    })
}
fn published_port(container: &str, port: &str) -> Result<u16> {
    docker_output([
        "inspect",
        "--format",
        &format!("{{{{(index (index .NetworkSettings.Ports \"{port}\") 0).HostPort}}}}"),
        container,
    ])?
    .parse()
    .context("Docker returned an invalid published port")
}
/// `docker inspect` fails the same way for a missing object and an unreachable daemon, so
/// existence comes from a filtered listing: empty means absent, failure stays an error.
fn docker_listed<const N: usize>(args: [&str; N]) -> Result<bool> {
    Ok(!docker_output(args)?.is_empty())
}
fn ensure_image(image: &str) -> Result<()> {
    if docker_output(["image", "inspect", image]).is_err() {
        println!("Pulling {image}…");
        docker(["pull", image])?;
    }
    Ok(())
}
fn build_project_images(dev: bool) -> Result<()> {
    let project_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    if !project_root.join("Dockerfile").is_file()
        || !project_root
            .join("docker/lightwalletd.Dockerfile")
            .is_file()
    {
        bail!(
            "cannot build images: project source is unavailable at {}",
            project_root.display()
        );
    }

    let app_image = app_image();
    let lightwalletd_image = lightwalletd_image();
    if dev {
        println!("Building {app_image} with the Rust development profile…");
        docker_inherit_in(
            &[
                "build",
                "--build-arg",
                "RUST_PROFILE=dev-runtime",
                "-t",
                &app_image,
                ".",
            ],
            &project_root,
        )?;
    } else {
        println!("Building {app_image}…");
        docker_inherit_in(&["build", "-t", &app_image, "."], &project_root)?;
    }
    println!("Building {lightwalletd_image}…");
    docker_inherit_in(
        &[
            "build",
            "-f",
            "docker/lightwalletd.Dockerfile",
            "-t",
            &lightwalletd_image,
            ".",
        ],
        &project_root,
    )
}
struct Shutdown {
    flag: Arc<AtomicBool>,
    signal: Arc<AtomicI32>,
    /// Lets a `DeletionMonitor` end `wait` through the same channel as Ctrl+C.
    sender: mpsc::Sender<Ended>,
    receiver: mpsc::Receiver<Ended>,
}

impl Shutdown {
    #[cfg(test)]
    fn channel() -> (mpsc::Sender<Ended>, Self) {
        let (sender, receiver) = mpsc::channel();
        let shutdown = Self {
            flag: Arc::new(AtomicBool::new(false)),
            signal: Arc::new(AtomicI32::new(0)),
            sender: sender.clone(),
            receiver,
        };
        (sender, shutdown)
    }

    fn install() -> Result<Self> {
        let (sender, receiver) = mpsc::channel();
        let flag = Arc::new(AtomicBool::new(false));
        let signal = Arc::new(AtomicI32::new(0));
        let handler_flag = flag.clone();
        let handler_signal = signal.clone();
        let handler_sender = sender.clone();
        let mut signals = signal_hook::iterator::Signals::new([
            signal_hook::consts::SIGINT,
            signal_hook::consts::SIGTERM,
        ])
        .context("installing shutdown signal handlers")?;
        thread::spawn(move || {
            for received in signals.forever() {
                handler_signal.store(received, Ordering::SeqCst);
                handler_flag.store(true, Ordering::SeqCst);
                if handler_sender.send(Ended::Interrupted).is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            flag,
            signal,
            sender,
            receiver,
        })
    }

    fn try_interrupted(&self) -> bool {
        // Deletion notices only arrive while `wait` runs, which receives them itself.
        if matches!(self.receiver.try_recv(), Ok(Ended::Interrupted)) {
            self.flag.store(true, Ordering::SeqCst);
        }
        self.flag.load(Ordering::SeqCst)
    }

    fn received_signal(&self) -> Option<i32> {
        if self.try_interrupted() {
            let signal = self.signal.load(Ordering::SeqCst);
            Some(if signal == 0 {
                signal_hook::consts::SIGTERM
            } else {
                signal
            })
        } else {
            None
        }
    }

    fn check(&self) -> Result<()> {
        if self.try_interrupted() {
            bail!("interrupted");
        }
        Ok(())
    }

    /// Waits for a shutdown signal, or for a `DeletionMonitor` to report that another command
    /// (`ths stop` or `ths reset` from another shell) deleted the environment.
    fn wait(&self) -> Result<Ended> {
        // Not `try_interrupted`: it would drop a deletion notice that is already queued.
        if self.flag.load(Ordering::SeqCst) {
            return Ok(Ended::Interrupted);
        }
        let ended = self
            .receiver
            .recv()
            .context("waiting for a shutdown signal")?;
        if ended == Ended::Interrupted {
            self.flag.store(true, Ordering::SeqCst);
        }
        Ok(ended)
    }

    fn wait_timeout(&self, timeout: Duration) -> Result<()> {
        if self.try_interrupted() {
            bail!("interrupted");
        }
        match self.receiver.recv_timeout(timeout) {
            Ok(_) => {
                self.flag.store(true, Ordering::SeqCst);
                bail!("interrupted");
            }
            Err(RecvTimeoutError::Timeout) => Ok(()),
            Err(RecvTimeoutError::Disconnected) => {
                if self.try_interrupted() {
                    bail!("interrupted");
                }
                Err(anyhow!("waiting for a shutdown signal"))
            }
        }
    }
}

/// How a running environment's foreground `start` ended.
#[derive(Debug, PartialEq, Eq)]
enum Ended {
    /// Ctrl+C or a termination signal: `start` deletes the environment itself.
    Interrupted,
    /// Another command already deleted it.
    DeletedElsewhere,
}

/// How long deletion monitoring waits before retrying after a Docker error or a lost event stream.
const DELETION_WATCH_RETRY: Duration = Duration::from_secs(1);

/// The container whose deletion a running `start` watches. Its ID keeps a same-name replacement
/// from hiding the deletion, and its creation time (Docker's clock) lets a subscription replay
/// a destroy event that happened before it connected.
#[derive(Clone, Debug, PartialEq, Eq)]
struct WatchedContainer {
    id: String,
    created: String,
}

/// The Docker operations behind deletion monitoring, and its test seam.
trait ContainerEvents: Send + Sync {
    /// Whether the container with this ID still exists.
    fn exists(&self, id: &str) -> Result<bool>;
    /// Blocks until Docker reports that the container was destroyed (`Ok(true)`), or until the
    /// event stream ends without reporting it (`Ok(false)`).
    fn wait_destroyed(&self, container: &WatchedContainer) -> Result<bool>;
    /// Ends a blocked `wait_destroyed` and makes later ones return at once.
    fn cancel(&self);
}

/// Follows the app container's lifecycle through `docker events`.
#[derive(Default)]
struct DockerEvents {
    cancelled: AtomicBool,
    listener: Mutex<Option<Child>>,
}

impl DockerEvents {
    fn stop_listener(&self) {
        let listener = self
            .listener
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(mut child) = listener {
            // `kill` fails only when the stream already ended; `wait` reaps the process either way.
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl ContainerEvents for DockerEvents {
    fn exists(&self, id: &str) -> Result<bool> {
        docker_listed([
            "container",
            "ls",
            "--all",
            "--quiet",
            "--filter",
            &format!("id={id}"),
        ])
    }

    fn wait_destroyed(&self, container: &WatchedContainer) -> Result<bool> {
        let stdout = {
            let mut listener = self.listener.lock().unwrap_or_else(PoisonError::into_inner);
            // Checked under the lock so `cancel` either sees this listener or stops it starting.
            if self.cancelled.load(Ordering::SeqCst) {
                return Ok(false);
            }
            let mut child = docker_cli()
                .args([
                    "events",
                    "--since",
                    &container.created,
                    "--filter",
                    &format!("container={}", container.id),
                    "--filter",
                    "event=destroy",
                    "--format",
                    "{{.Action}}",
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .context("running Docker")?;
            let stdout = child
                .stdout
                .take()
                .context("reading Docker container events")?;
            *listener = Some(child);
            stdout
        };
        let destroyed = BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
            .any(|action| action.trim() == "destroy");
        self.stop_listener();
        Ok(destroyed)
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.stop_listener();
    }
}

/// Returns whether the allocated container was deleted, or `false` once `stop` disconnects.
/// A Docker error never counts as deletion: the state is unknown, so it retries after `retry`.
fn watch_deletion(
    events: &dyn ContainerEvents,
    container: &WatchedContainer,
    retry: Duration,
    stop: &mpsc::Receiver<()>,
) -> bool {
    let stopped = || !matches!(stop.recv_timeout(retry), Err(RecvTimeoutError::Timeout));
    loop {
        if matches!(events.wait_destroyed(container), Ok(true)) {
            return true;
        }
        if !matches!(stop.try_recv(), Err(TryRecvError::Empty)) {
            return false;
        }
        // The stream ended or failed, for example when the daemon restarted. Its buffered
        // events may be gone, so check the container itself before subscribing again.
        match events.exists(&container.id) {
            Ok(false) => return true,
            Ok(true) | Err(_) if stopped() => return false,
            Ok(true) | Err(_) => {}
        }
    }
}

/// Watches the app container on a background thread and reports its deletion to `Shutdown`.
/// Dropping it stops the watch and reaps its `docker events` process.
struct DeletionMonitor {
    events: Arc<dyn ContainerEvents>,
    stop: Option<mpsc::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl DeletionMonitor {
    fn spawn(
        events: Arc<dyn ContainerEvents>,
        app_container: &WatchedContainer,
        retry: Duration,
        shutdown: &Shutdown,
    ) -> Self {
        let (stop, stopped) = mpsc::channel();
        let watcher = events.clone();
        let container = app_container.clone();
        let notify = shutdown.sender.clone();
        let thread = thread::spawn(move || {
            if watch_deletion(watcher.as_ref(), &container, retry, &stopped) {
                // Fails only once `start` has stopped listening, when the notice no longer matters.
                let _ = notify.send(Ended::DeletedElsewhere);
            }
        });
        Self {
            events,
            stop: Some(stop),
            thread: Some(thread),
        }
    }
}

impl Drop for DeletionMonitor {
    fn drop(&mut self) {
        drop(self.stop.take());
        self.events.cancel();
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            eprintln!("deletion monitoring stopped unexpectedly");
        }
    }
}

trait StartHost {
    fn instance_exists(
        &self,
        runtime: &Runtime,
        name: &InstanceName,
        _shutdown: &Shutdown,
        _context: &LifecycleContext,
    ) -> Result<bool> {
        Ok(runtime.instance_dir(name).exists())
    }
    fn runtime_status(&self, endpoints: &Endpoints, timeout: Duration) -> Result<RuntimeStatus> {
        runtime_status(endpoints, timeout)
    }
    fn run_child(
        &self,
        command: &[OsString],
        current_dir: &Path,
        name: &InstanceName,
        endpoints: &Endpoints,
        shutdown: &Shutdown,
    ) -> Result<ChildOutcome> {
        run_child(
            command,
            current_dir,
            name,
            endpoints,
            shutdown,
            Duration::from_secs(5),
        )
    }
    fn diagnostics(&self, _name: &InstanceName, _context: &LifecycleContext) -> String {
        String::new()
    }
    fn delete(
        &self,
        runtime: &Runtime,
        name: &InstanceName,
        context: &LifecycleContext,
        deadline: Deadline,
    ) -> Result<CleanupReport>;
    fn delete_partial(
        &self,
        runtime: &Runtime,
        name: &InstanceName,
        context: &LifecycleContext,
        deadline: Deadline,
    ) -> Result<CleanupReport> {
        self.delete(runtime, name, context, deadline)
    }
    fn allocate(
        &self,
        runtime: &Runtime,
        name: &InstanceName,
        shutdown: &Shutdown,
        port_offset: u16,
        context: &LifecycleContext,
        journal: &mut recovery::RecoveryJournal,
    ) -> Result<AllocatedInstance>;
    fn wait_ready(
        &self,
        allocated: &AllocatedInstance,
        shutdown: &Shutdown,
        context: &LifecycleContext,
    ) -> Result<()>;
    fn open_url(&self, url: &str, shutdown: &Shutdown, context: &LifecycleContext) -> Result<()>;
    fn wait_for_shutdown(
        &self,
        app_container: &WatchedContainer,
        shutdown: &Shutdown,
    ) -> Result<Ended>;
}

struct DockerHost;

impl StartHost for DockerHost {
    fn diagnostics(&self, name: &InstanceName, context: &LifecycleContext) -> String {
        let deadline = Deadline::after(Duration::from_secs(5));
        let docker = LifecycleDocker::production(
            context,
            Some(deadline),
            context.policy.readiness_docker,
            Cancellation::Ignore,
        );
        ["app", "zakura", "lightwalletd"]
            .map(|service| {
                let container = format!("{}-{service}", prefix(name));
                let logs = readiness::diagnostic_logs(&docker, &container)
                    .unwrap_or_else(|error| format!("could not read logs: {error:#}"));
                format!("\n--- {service} ---\n{logs}")
            })
            .join("")
    }

    fn instance_exists(
        &self,
        runtime: &Runtime,
        name: &InstanceName,
        shutdown: &Shutdown,
        context: &LifecycleContext,
    ) -> Result<bool> {
        if runtime.instance_dir(name).exists() {
            return Ok(true);
        }
        let requested = || shutdown.try_interrupted();
        let docker = LifecycleDocker::production(
            context,
            context.overall_deadline,
            context.policy.startup_docker,
            Cancellation::Observe(&requested),
        );
        let label_filter = format!("label={}", label(name));
        let network_name = format!("name=^{}$", prefix(name));
        for args in [
            vec![
                "container",
                "ls",
                "--all",
                "--filter",
                &label_filter,
                "--format",
                "{{.ID}}",
            ],
            vec![
                "volume",
                "ls",
                "--filter",
                &label_filter,
                "--format",
                "{{.Name}}",
            ],
            vec![
                "network",
                "ls",
                "--filter",
                &network_name,
                "--format",
                "{{.Name}}",
            ],
        ] {
            if !docker.output(&args)?.is_empty() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn delete(
        &self,
        runtime: &Runtime,
        name: &InstanceName,
        context: &LifecycleContext,
        deadline: Deadline,
    ) -> Result<CleanupReport> {
        let docker = lifecycle_docker(context, deadline, Cancellation::Ignore);
        runtime.cleanup_core(name, &docker, false, deadline, context)
    }

    fn delete_partial(
        &self,
        runtime: &Runtime,
        name: &InstanceName,
        context: &LifecycleContext,
        deadline: Deadline,
    ) -> Result<CleanupReport> {
        let docker = lifecycle_docker(context, deadline, Cancellation::Ignore);
        runtime.cleanup_core(name, &docker, true, deadline, context)
    }

    fn allocate(
        &self,
        runtime: &Runtime,
        name: &InstanceName,
        shutdown: &Shutdown,
        port_offset: u16,
        context: &LifecycleContext,
        journal: &mut recovery::RecoveryJournal,
    ) -> Result<AllocatedInstance> {
        let requested = || shutdown.try_interrupted();
        let docker = LifecycleDocker::production(
            context,
            context.overall_deadline,
            context.policy.startup_docker,
            Cancellation::Observe(&requested),
        );
        fs::create_dir_all(runtime.instance_dir(name))?;
        shutdown.check()?;
        let prefix = prefix(name);
        let ports = host_ports(port_offset)?;
        require_free_loopback(ports.dashboard)?;
        require_free_loopback(ports.rpc)?;
        require_free_loopback(ports.p2p)?;
        require_free_loopback(ports.lightwalletd)?;
        shutdown.check()?;
        let network_budget = context.startup_deadline(context.policy.startup_docker);
        let network = ensure_network_with(
            &prefix,
            name,
            &docker.scoped(network_budget, context.policy.startup_docker),
            journal,
            network_budget,
        )?;
        shutdown.check()?;
        let mut volumes = Vec::new();
        for suffix in ["chain", "wallet", "lightwalletd", "config"] {
            let budget = context.startup_deadline(context.policy.startup_docker);
            volumes.push(ensure_volume_with(
                &format!("{prefix}-{suffix}"),
                name,
                &docker.scoped(budget, context.policy.startup_docker),
                journal,
                budget,
            )?);
            shutdown.check()?;
        }
        let init_name = format!("{prefix}-init");
        let init_budget = context.startup_deadline(context.policy.startup_docker);
        let init_docker = docker.scoped(init_budget, context.policy.startup_docker);
        if owned_resource(&init_docker, "container", &init_name, name)?.is_none() {
            let app = app_image();
            let init_id = create_container(
                &init_name,
                name,
                &init_docker,
                journal,
                init_budget,
                volumes.clone(),
                &[
                    "create",
                    "--name",
                    &init_name,
                    "--label",
                    &label(name),
                    "-v",
                    &format!("{prefix}-wallet:/data"),
                    "-v",
                    &format!("{prefix}-config:/config"),
                    &app,
                    "init",
                    "--data-dir",
                    "/data",
                    "--config-dir",
                    "/config",
                ],
            )?;
            shutdown.check()?;
            let start_budget = context.startup_deadline(context.policy.initialization);
            let starter = docker.scoped(start_budget, context.policy.initialization);
            let index = journal.begin(
                recovery::ResourceRef {
                    kind: recovery::ResourceKind::Container,
                    name: init_name.clone(),
                    identity: Some(init_id.clone()),
                },
                volumes.clone(),
                recovery::MutationOperation::Start,
            )?;
            if let Err(error) = starter.run(&["start", "-a", &init_id]) {
                return Err(mutation_failure(journal, index, error, Some(init_id)));
            }
            journal.finish(
                index,
                recovery::MutationOutcome::Acknowledged,
                Some(init_id),
            )?;
            shutdown.check()?;
        }
        let zakura_budget = context.startup_deadline(context.policy.startup_docker);
        let zakura_id = ensure_zakura(
            &prefix,
            name,
            &ports,
            &docker.scoped(zakura_budget, context.policy.startup_docker),
            journal,
            zakura_budget,
            vec![network.clone()],
        )?;
        shutdown.check()?;
        let lightwalletd_budget = context.startup_deadline(context.policy.startup_docker);
        let lightwalletd_id = ensure_lightwalletd(
            &prefix,
            name,
            &ports,
            &docker.scoped(lightwalletd_budget, context.policy.startup_docker),
            journal,
            lightwalletd_budget,
            vec![network.clone()],
        )?;
        shutdown.check()?;
        let zakura_start = context.startup_deadline(context.policy.startup_docker);
        start_container(
            &zakura_id,
            &format!("{prefix}-zakura"),
            &docker.scoped(zakura_start, context.policy.startup_docker),
            journal,
            zakura_start,
        )?;
        shutdown.check()?;
        let zakura_rpc = format!(
            "http://127.0.0.1:{}",
            published_port_with(
                &docker.scoped(zakura_start, context.policy.startup_docker),
                &zakura_id,
                "18232/tcp",
            )?
        );
        let tip_deadline = context.startup_deadline(context.policy.readiness);
        readiness::wait_for_zakura_tip(
            &zakura_rpc,
            &zakura_id,
            tip_deadline,
            shutdown,
            &context.policy,
            &docker.scoped(tip_deadline, context.policy.readiness_docker),
        )?;
        let lightwalletd_start = context.startup_deadline(context.policy.startup_docker);
        start_container(
            &lightwalletd_id,
            &format!("{prefix}-lightwalletd"),
            &docker.scoped(lightwalletd_start, context.policy.startup_docker),
            journal,
            lightwalletd_start,
        )?;
        shutdown.check()?;
        let app_budget = context.startup_deadline(context.policy.startup_docker);
        let app_container = ensure_app(
            &prefix,
            name,
            &ports,
            &docker.scoped(app_budget, context.policy.startup_docker),
            journal,
            app_budget,
            vec![network],
        )?;
        shutdown.check()?;
        let app_start = context.startup_deadline(context.policy.startup_docker);
        start_container(
            &app_container.id,
            &format!("{prefix}-app"),
            &docker.scoped(app_start, context.policy.startup_docker),
            journal,
            app_start,
        )?;
        shutdown.check()?;
        let endpoints = endpoints_for(&ports);
        runtime.write_instance(name, &endpoints)?;
        Ok(AllocatedInstance {
            endpoints,
            resources: journal.record().resources.clone(),
            app_container,
            zakura_container_id: zakura_id,
        })
    }

    fn wait_ready(
        &self,
        allocated: &AllocatedInstance,
        shutdown: &Shutdown,
        context: &LifecycleContext,
    ) -> Result<()> {
        let requested = || shutdown.try_interrupted();
        let deadline = context.startup_deadline(context.policy.readiness);
        let docker = LifecycleDocker::production(
            context,
            Some(deadline),
            context.policy.readiness_docker,
            Cancellation::Observe(&requested),
        );
        readiness::wait_ready(
            &allocated.endpoints.dashboard,
            &allocated.app_container.id,
            deadline,
            shutdown,
            &context.policy,
            &docker,
        )
    }

    fn open_url(&self, url: &str, shutdown: &Shutdown, context: &LifecycleContext) -> Result<()> {
        open_url_with(url, shutdown, context)
    }

    fn wait_for_shutdown(
        &self,
        app_container: &WatchedContainer,
        shutdown: &Shutdown,
    ) -> Result<Ended> {
        let _monitor = DeletionMonitor::spawn(
            Arc::new(DockerEvents::default()),
            app_container,
            DELETION_WATCH_RETRY,
            shutdown,
        );
        shutdown.wait()
    }
}

fn lifecycle_docker<'a>(
    context: &'a LifecycleContext,
    deadline: Deadline,
    cancellation: Cancellation<'a>,
) -> LifecycleDocker<'a> {
    // The cleanup allowance's termination reserve stays outside every Docker
    // call so HelperSet::finish can still reap inside the original deadline.
    let operation_deadline = deadline.saturating_sub(context.policy.termination_reserve);
    LifecycleDocker::production(
        context,
        Some(operation_deadline),
        context.policy.startup_docker,
        cancellation,
    )
}

struct CleanupOnDrop<'a> {
    runtime: &'a Runtime,
    name: &'a InstanceName,
    host: &'a dyn StartHost,
    context: &'a LifecycleContext,
    armed: bool,
}

impl Drop for CleanupOnDrop<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let deadline = Deadline::after(self.context.policy.cleanup);
        if let Err(error) =
            self.host
                .delete_partial(self.runtime, self.name, self.context, deadline)
        {
            eprintln!("could not delete {}: {error:#}", self.name);
        }
    }
}

impl Runtime {
    #[cfg(test)]
    fn start_with(
        &self,
        name: &InstanceName,
        no_open: bool,
        json: bool,
        port_offset: u16,
        host: &dyn StartHost,
        shutdown: &Shutdown,
    ) -> Result<()> {
        let context = LifecycleContext::new(LifecyclePolicy::default());
        self.start_with_policy(name, no_open, json, port_offset, host, shutdown, &context)
    }

    #[allow(clippy::too_many_arguments)]
    fn start_with_policy(
        &self,
        name: &InstanceName,
        no_open: bool,
        json: bool,
        port_offset: u16,
        host: &dyn StartHost,
        shutdown: &Shutdown,
        context: &LifecycleContext,
    ) -> Result<()> {
        host_ports(port_offset)?;
        let mut lifecycle = Some(self.lock_instance(name)?);
        let mut cleanup = CleanupOnDrop {
            runtime: self,
            name,
            host,
            context,
            armed: false,
        };
        println!("Preparing a fresh {name} environment…");
        let initial_deadline = Deadline::after(context.policy.cleanup);
        let initial = host.delete(self, name, context, initial_deadline)?;
        if !initial.verified() {
            return initial.into_result();
        }
        cleanup.armed = true;
        println!("Starting {name}…");
        let primary = (|| {
            fs::create_dir_all(self.instance_dir(name))?;
            let mut journal = recovery::RecoveryJournal::create(self.recovery_path(name), name)?;
            let allocated =
                host.allocate(self, name, shutdown, port_offset, context, &mut journal)?;
            if allocated.zakura_container_id.is_empty()
                || allocated
                    .resources
                    .iter()
                    .any(|resource| resource.name.is_empty())
            {
                bail!("startup did not record the allocated resource identities");
            }
            shutdown.check()?;
            host.wait_ready(&allocated, shutdown, context)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&allocated.endpoints)?);
            } else {
                println!(
                    "\n{name} is ready 🌸\n{}",
                    endpoint_lines(&allocated.endpoints)
                );
            }
            if !no_open {
                host.open_url(&allocated.endpoints.dashboard, shutdown, context)?;
            }
            if !json {
                println!("\nPress Ctrl+C to stop and delete this development environment.");
            }
            drop(lifecycle.take());
            host.wait_for_shutdown(&allocated.app_container, shutdown)
        })();
        cleanup.armed = false;
        if matches!(primary, Ok(Ended::DeletedElsewhere)) {
            println!("\n{name} was stopped and deleted by another command.");
            return Ok(());
        }
        if primary.is_ok() {
            println!("\nStopping and deleting {name}…");
        }
        let final_deadline = Deadline::after(context.policy.cleanup);
        let cleaned = host
            .delete_partial(self, name, context, final_deadline)
            .and_then(CleanupReport::into_result);
        match (primary, cleaned) {
            (Ok(_), Ok(())) => {
                println!("Deleted {name} and all of its development data.");
                Ok(())
            }
            (Err(primary), Ok(())) => Err(primary),
            (Ok(_), Err(error)) => Err(error),
            (Err(primary), Err(error)) => Err(anyhow!("{primary:#}\n{error:#}")),
        }
    }

    fn run_with_policy(
        &self,
        name: &InstanceName,
        command: &[OsString],
        current_dir: &Path,
        host: &dyn StartHost,
        shutdown: &Shutdown,
        context: &LifecycleContext,
    ) -> Result<u8> {
        host_ports(0)?;
        let _lifecycle = self.lock_instance(name)?;
        let deadline = context.overall_deadline.context("missing run deadline")?;
        if deadline.expired() {
            bail!("environment did not start before the timeout");
        }
        if host.instance_exists(self, name, shutdown, context)? {
            bail!("environment {name} already exists");
        }
        if deadline.expired() {
            bail!("environment did not start before the timeout");
        }
        fs::create_dir_all(&self.root)?;
        fs::create_dir(self.instance_dir(name))
            .with_context(|| format!("reserving fresh environment {name}"))?;
        let mut cleanup = CleanupOnDrop {
            runtime: self,
            name,
            host,
            context,
            armed: true,
        };
        println!("Preparing a fresh {name} environment…");
        let primary = (|| {
            let mut journal = recovery::RecoveryJournal::create(self.recovery_path(name), name)?;
            let allocated = host.allocate(self, name, shutdown, 0, context, &mut journal)?;
            if allocated.zakura_container_id.is_empty()
                || allocated
                    .resources
                    .iter()
                    .any(|resource| resource.name.is_empty())
            {
                bail!("startup did not record the allocated resource identities");
            }
            shutdown.check()?;
            if deadline.expired() {
                bail!("environment did not start before the timeout");
            }
            host.wait_ready(&allocated, shutdown, context)?;
            shutdown.check()?;
            if deadline.expired() {
                bail!("environment did not become ready before the timeout");
            }
            wait_until_settled(
                &|remaining| host.runtime_status(&allocated.endpoints, remaining),
                deadline.0,
                Duration::from_millis(750),
                shutdown,
                true,
            )?;
            println!("{name} is settled; running child command…");
            host.run_child(command, current_dir, name, &allocated.endpoints, shutdown)
        })();
        if matches!(&primary, Ok(ChildOutcome::Exited(code)) if *code != 0)
            || matches!(&primary, Ok(ChildOutcome::Signaled))
        {
            let diagnostics = host.diagnostics(name, context);
            if !diagnostics.is_empty() {
                eprintln!("Run diagnostics:{diagnostics}");
            }
        }
        cleanup.armed = false;
        let cleaned = host
            .delete_partial(self, name, context, Deadline::after(context.policy.cleanup))
            .and_then(CleanupReport::into_result);
        match (primary, cleaned) {
            (Ok(ChildOutcome::Exited(code)), Ok(())) => Ok(code),
            (Ok(ChildOutcome::Signaled), Ok(())) => Ok(1),
            (Err(primary), Ok(())) => Err(primary),
            (Ok(ChildOutcome::Exited(0)), Err(cleanup)) => Err(cleanup),
            (Ok(ChildOutcome::Exited(code)), Err(cleanup)) => {
                eprintln!("could not delete {name}: {cleanup:#}");
                Ok(code)
            }
            (Ok(ChildOutcome::Signaled), Err(cleanup)) => {
                eprintln!("could not delete {name}: {cleanup:#}");
                Ok(1)
            }
            (Err(primary), Err(cleanup)) => Err(anyhow!("{primary:#}\n{cleanup:#}")),
        }
    }
}

fn display_height(height: Option<u64>) -> String {
    height.map_or_else(|| "unavailable".to_owned(), |height| height.to_string())
}

fn wait_until_settled(
    read_status: &dyn Fn(Duration) -> Result<RuntimeStatus>,
    deadline: Instant,
    poll_interval: Duration,
    shutdown: &Shutdown,
    progress: bool,
) -> Result<RuntimeStatus> {
    let mut last = None;
    let mut last_error = None;
    loop {
        shutdown.check()?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            let mut detail = last.as_ref().map_or_else(
                || "no status received".to_owned(),
                |status: &RuntimeStatus| {
                    format!(
                        "node {}, lightwalletd {}, wallet {}, blocker {}",
                        display_height(status.node_height),
                        display_height(status.lightwalletd_height),
                        display_height(status.wallet_scanned_height),
                        status.settlement_blocker.as_deref().unwrap_or("unknown")
                    )
                },
            );
            if let Some(error) = last_error {
                detail.push_str(&format!("; last status read failed: {error}"));
            }
            bail!("environment did not settle before the timeout ({detail})");
        }
        let status = match read_status(remaining.min(Duration::from_secs(5))) {
            Ok(status) => status,
            Err(error) => {
                let error = format!("{error:#}");
                if progress && last_error.as_deref() != Some(error.as_str()) {
                    eprintln!("Waiting: runtime status unavailable ({error})");
                }
                last_error = Some(error);
                shutdown.wait_timeout(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(poll_interval),
                )?;
                continue;
            }
        };
        last_error = None;
        if Instant::now() >= deadline {
            continue;
        }
        if status.settled {
            return Ok(status);
        }
        if progress && last.as_ref() != Some(&status) {
            eprintln!(
                "Waiting: node {}, lightwalletd {}, wallet {} ({})",
                display_height(status.node_height),
                display_height(status.lightwalletd_height),
                display_height(status.wallet_scanned_height),
                status
                    .settlement_blocker
                    .as_deref()
                    .unwrap_or("not settled")
            );
        }
        last = Some(status);
        shutdown.wait_timeout(
            deadline
                .saturating_duration_since(Instant::now())
                .min(poll_interval),
        )?;
    }
}

fn runtime_status(endpoints: &Endpoints, timeout: Duration) -> Result<RuntimeStatus> {
    let response = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()?
        .get(format!("{}/api/v1/status", endpoints.dashboard))
        .send()
        .context("reading runtime status")?;
    let status = response.status();
    if !status.is_success() {
        bail!("runtime status returned {status}");
    }
    response.json().context("decoding runtime status")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildOutcome {
    Exited(u8),
    Signaled,
}

fn run_child(
    command: &[OsString],
    current_dir: &Path,
    name: &InstanceName,
    endpoints: &Endpoints,
    shutdown: &Shutdown,
    shutdown_grace: Duration,
) -> Result<ChildOutcome> {
    let (program, arguments) = command
        .split_first()
        .context("run requires a child command")?;
    let mut process = Command::new(program);
    process
        .args(arguments)
        .current_dir(current_dir)
        .env("THS_INSTANCE", name.to_string())
        .env("THS_NETWORK", "regtest")
        .env("THS_API_URL", &endpoints.dashboard)
        .env("THS_ZAKURA_RPC_URL", &endpoints.rpc)
        .env("THS_LIGHTWALLETD_URL", &endpoints.lightwalletd)
        .env("THS_LIGHTWALLETD_TLS", "false")
        .env("THS_P2P_ADDR", &endpoints.p2p);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        process.process_group(0);
    }
    let mut child = process
        .spawn()
        .with_context(|| format!("starting child command {}", program.to_string_lossy()))?;
    loop {
        if let Some(status) = child.try_wait().context("waiting for child command")? {
            return Ok(
                match status.code().and_then(|code| u8::try_from(code).ok()) {
                    Some(code) => ChildOutcome::Exited(code),
                    None => ChildOutcome::Signaled,
                },
            );
        }
        if let Some(signal) = shutdown.received_signal() {
            terminate_child_group(&mut child, signal, shutdown_grace)?;
            return Ok(ChildOutcome::Signaled);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(unix)]
fn terminate_child_group(
    child: &mut std::process::Child,
    signal: i32,
    grace: Duration,
) -> Result<()> {
    use nix::{
        errno::Errno,
        sys::signal::{Signal, killpg},
        unistd::Pid,
    };

    let pgid = Pid::from_raw(i32::try_from(child.id()).context("child process ID is too large")?);
    let signal = Signal::try_from(signal).context("unsupported shutdown signal")?;
    if let Err(error) = killpg(pgid, signal)
        && error != Errno::ESRCH
    {
        return Err(error).context("forwarding shutdown signal to child process group");
    }

    let deadline = Instant::now() + grace;
    let mut reaped = false;
    loop {
        if !reaped
            && child
                .try_wait()
                .context("waiting for child command")?
                .is_some()
        {
            reaped = true;
        }
        match killpg(pgid, None) {
            Err(Errno::ESRCH) => break,
            Ok(()) | Err(Errno::EPERM) => {}
            Err(error) => return Err(error).context("checking child process group"),
        }
        if Instant::now() >= deadline {
            killpg(pgid, Signal::SIGKILL)
                .or_else(|error| (error == Errno::ESRCH).then_some(()).ok_or(error))
                .context("force-terminating child process group")?;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    if !reaped {
        child.wait().context("reaping child command")?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn terminate_child_group(
    child: &mut std::process::Child,
    _signal: i32,
    _grace: Duration,
) -> Result<()> {
    child.kill().context("terminating child command")?;
    child.wait().context("reaping child command")?;
    Ok(())
}

/// Lists only running containers, so a stopped or missing one is `false` rather than an error.
fn container_running(name: &str) -> Result<bool> {
    docker_listed([
        "container",
        "ls",
        "--quiet",
        "--filter",
        &format!("name=^{name}$"),
    ])
}
fn published_port_with(
    docker: &impl DockerResourceCommands,
    container: &str,
    port: &str,
) -> Result<u16> {
    docker
        .output(&[
            "inspect",
            "--format",
            &format!("{{{{(index (index .NetworkSettings.Ports \"{port}\") 0).HostPort}}}}"),
            container,
        ])?
        .parse()
        .context("Docker returned an invalid published port")
}

fn startup_docker_reachable(docker: &impl DockerResourceCommands) -> Result<()> {
    let version = docker
        .output(&["version", "--format", "{{.Server.Version}}"])
        .context("Docker is not reachable; start Docker Desktop or the Docker daemon")?;
    println!("✓ Docker {version}");
    Ok(())
}

fn startup_require_image(docker: &LifecycleDocker<'_>, image: &str) -> Result<()> {
    match docker.execute(
        &["image", "inspect", "--format", "{{.Id}}", image],
        OutputMode::Capture,
    ) {
        Ok(output) if !output.stdout_truncated && !output.stderr_truncated => Ok(()),
        Ok(_) => bail!("checking image {image} returned truncated output"),
        Err(error) if error.kind == CommandFailureKind::Nonzero => {
            confirm_image_absent(docker, image, &error.stderr)
        }
        Err(error) => Err(anyhow!("{error}").context(format!("checking image {image}"))),
    }
}

fn confirm_image_absent(
    docker: &LifecycleDocker<'_>,
    image: &str,
    inspect_stderr: &[u8],
) -> Result<()> {
    match docker.execute(
        &["image", "ls", "--format", "{{.Repository}}:{{.Tag}}"],
        OutputMode::Capture,
    ) {
        Ok(listing) if !listing.stdout_truncated => {
            let present = String::from_utf8_lossy(&listing.stdout)
                .lines()
                .any(|line| line.trim() == image);
            if present {
                Ok(())
            } else {
                bail!(
                    "required image {image} is unavailable; run `ths pull` (or `ths build` from a source checkout) first"
                )
            }
        }
        Ok(listing) => bail!(
            "checking image {image} failed: {}; image listing was not usable: {}",
            String::from_utf8_lossy(inspect_stderr).trim(),
            String::from_utf8_lossy(&listing.stderr).trim()
        ),
        Err(error) => Err(anyhow!("{error}").context(format!(
            "checking image {image} failed: {}",
            String::from_utf8_lossy(inspect_stderr).trim()
        ))),
    }
}

fn open_url_with(url: &str, shutdown: &Shutdown, context: &LifecycleContext) -> Result<()> {
    let (program, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else {
        ("xdg-open", vec![url])
    };
    let requested = || shutdown.try_interrupted();
    let mut command = Command::new(program);
    command.args(args);
    let deadline = Deadline::after(context.policy.startup_docker);
    let mut helpers = context.helpers.borrow_mut();
    run(
        &mut command,
        deadline,
        Cancellation::Observe(&requested),
        OutputMode::Null,
        &context.policy,
        &mut helpers,
    )
    .map_err(|error| anyhow!("{error}"))
    .with_context(|| format!("opening {url}"))?;
    drop(helpers);
    shutdown.check()?;
    Ok(())
}
fn status_text(name: &InstanceName, running: bool, e: &Endpoints) -> String {
    let state = if running { "running" } else { "stopped" };
    format!("{name}: {state}\n{}", endpoint_lines(e))
}
fn endpoint_lines(e: &Endpoints) -> String {
    format!(
        "  Dashboard    {}\n  Zakura RPC   {}\n  lightwalletd {}  (network={}, tls={})\n  P2P          {}",
        e.dashboard, e.rpc, e.lightwalletd, e.network, e.tls, e.p2p
    )
}
/// keeps an environment whose containers can't be read, such as while docker is stopped, with
/// its live status unknown.
fn environment_status(
    instance: Instance,
    containers: Result<Vec<ContainerStatus>>,
) -> EnvironmentStatus {
    let (state, containers) = match containers {
        Ok(containers) => (environment_state(&containers), containers),
        Err(_) => ("unknown", Vec::new()),
    };
    EnvironmentStatus {
        state,
        instance,
        containers,
    }
}
/// an environment's state from its long-running services; `init` is not one of them. a paused
/// service is still up, so it never makes an environment stopped.
fn environment_state(containers: &[ContainerStatus]) -> &'static str {
    if containers.iter().any(|c| c.state == "foreign") {
        return "conflict";
    }
    let services: Vec<_> = containers.iter().filter(|c| c.service != "init").collect();
    let up = services
        .iter()
        .filter(|c| c.state == "running" || c.state == "paused")
        .count();
    match up {
        0 => "stopped",
        n if n < services.len() => "degraded",
        _ if services.iter().any(|c| c.state == "paused") => "paused",
        _ => "running",
    }
}
fn container_status(
    docker: &impl DockerResourceCommands,
    name: &InstanceName,
    service: &'static str,
) -> Result<ContainerStatus> {
    let container = format!("{}-{service}", prefix(name));
    let unread = |state: &str| ContainerStatus {
        service,
        state: state.into(),
        created_at: None,
        started_at: None,
    };
    let id = match owned_resource(docker, "container", &container, name) {
        Ok(Some(id)) => id,
        Ok(None) => return Ok(unread("missing")),
        Err(error) if error.to_string().contains("is not owned by") => {
            return Ok(unread("foreign"));
        }
        Err(error) => return Err(error),
    };
    let output = docker.output(&[
        "container",
        "inspect",
        "--format",
        "{{.State.Status}} {{.Created}} {{.State.StartedAt}}",
        &id,
    ])?;
    let mut fields = output.split_whitespace();
    Ok(ContainerStatus {
        service,
        state: fields.next().unwrap_or("unknown").to_owned(),
        created_at: fields.next().and_then(parse_docker_time),
        started_at: fields.next().and_then(parse_docker_time),
    })
}
/// unix seconds from docker's utc rfc 3339 timestamps. docker reports a container that never
/// started as started in year one, which gives nothing.
fn parse_docker_time(value: &str) -> Option<u64> {
    let (date, time) = value.split_once('T')?;
    let mut date = date.splitn(3, '-').map(str::parse::<i64>);
    let (year, month, day) = (date.next()?.ok()?, date.next()?.ok()?, date.next()?.ok()?);
    let time = time.strip_suffix('Z')?;
    let mut time = time.split(['.', ':']).map(str::parse::<i64>);
    let (hour, minute, second) = (time.next()?.ok()?, time.next()?.ok()?, time.next()?.ok()?);
    if year < 1970 {
        return None;
    }
    // days since the epoch for a proleptic gregorian date.
    let (y, m) = if month <= 2 {
        (year - 1, month + 9)
    } else {
        (year, month - 3)
    };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + hour * 3_600 + minute * 60 + second).ok()
}
fn now_unix() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before 1970")?
        .as_secs())
}
fn ago(now: u64, then: u64) -> String {
    let seconds = now.saturating_sub(then);
    match seconds {
        0..60 => format!("{seconds}s ago"),
        60..3_600 => format!("{}m ago", seconds / 60),
        3_600..86_400 => format!("{}h ago", seconds / 3_600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}
fn render_list(environments: &[EnvironmentStatus], now: u64) -> String {
    let (unknown, known): (Vec<_>, Vec<_>) =
        environments.iter().partition(|e| e.state == "unknown");
    let (conflict, known): (Vec<_>, Vec<_>) =
        known.into_iter().partition(|e| e.state == "conflict");
    let (active, inactive): (Vec<_>, Vec<_>) =
        known.into_iter().partition(|e| e.state != "stopped");
    let has_inactive = !inactive.is_empty();
    let mut out = String::new();
    for (title, section) in [
        ("ACTIVE", active),
        ("CONFLICT", conflict),
        ("UNKNOWN", unknown),
        ("INACTIVE", inactive),
    ] {
        if section.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(title);
        out.push('\n');
        for e in section {
            let mut line = format!(
                "{:<16} {:<26} {:<8}",
                e.instance.name, e.instance.endpoints.dashboard, e.state
            );
            if let Some(created) = e.instance.created_at {
                line.push_str(&format!("   created {}", ago(now, created)));
            }
            out.push_str(line.trim_end());
            out.push('\n');
            for c in &e.containers {
                let mut line = format!("  {:<13} {:<9}", c.service, c.state);
                if let Some(created) = c.created_at {
                    line.push_str(&format!(" created {:<9}", ago(now, created)));
                }
                if let Some(started) = c.started_at {
                    line.push_str(&format!(" started {}", ago(now, started)));
                }
                out.push_str(line.trim_end());
                out.push('\n');
            }
        }
    }
    if has_inactive {
        out.push_str(
            "\nRemove an inactive environment and all of its data with:\n  ths --name <name> stop\n",
        );
    }
    out
}
fn account_balances(
    dashboard: &str,
    client: &reqwest::blocking::Client,
    name: &InstanceName,
) -> Result<Vec<Account>> {
    let status: ServerStatus = client
        .get(format!("{dashboard}/api/v1/status"))
        .send()
        .map_err(anyhow::Error::from)
        .and_then(decode_response)
        .with_context(|| format!("checking wallet sync for environment {name}"))?;
    match status.wallet_sync.state.as_str() {
        "ready" => {}
        "syncing" => bail!("wallet for environment {name} is still syncing; try again later"),
        "error" => bail!(
            "wallet synchronization failed for environment {name}: {}",
            status
                .wallet_sync
                .error
                .as_deref()
                .unwrap_or("no error detail was provided")
        ),
        state => bail!("wallet for environment {name} has unknown sync state `{state}`"),
    }
    client
        .get(format!("{dashboard}/api/v1/accounts"))
        .send()
        .map_err(anyhow::Error::from)
        .and_then(decode_response)
        .with_context(|| format!("asking environment {name} for account balances"))
}
fn balance_lines(accounts: &[Account]) -> String {
    accounts
        .iter()
        .map(|account| {
            format!(
                "Account {}\n  Transparent  {} ZEC  {}\n  Ironwood     {} ZEC  {}",
                account.id,
                format_zec(account.transparent_zatoshi),
                account.transparent_address,
                format_zec(account.ironwood_zatoshi),
                account.unified_address
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}
fn open_url(url: &str) -> Result<()> {
    let (program, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else {
        ("xdg-open", vec![url])
    };
    Command::new(program)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("opening {url}"))?;
    Ok(())
}
fn docker_cli() -> Command {
    #[allow(unused_mut)]
    let mut command = Command::new("docker");
    #[cfg(test)]
    if tests::DOCKER_UNREACHABLE.get() {
        command.env("DOCKER_HOST", "unix:///nonexistent/ths-tests/docker.sock");
    }
    command
}
fn docker<const N: usize>(args: [&str; N]) -> Result<()> {
    docker_inherit(&args)
}
fn logs_args(container: &str, follow: bool, tail: Option<u64>) -> Vec<String> {
    let mut args = vec!["logs".to_owned()];
    if follow {
        args.push("--follow".to_owned());
    }
    if let Some(lines) = tail {
        args.extend(["--tail".to_owned(), lines.to_string()]);
    }
    args.push(container.to_owned());
    args
}

/// docker has no head option, so stream both of its output streams through
/// one pipe and stop the `docker logs` process once enough lines have been printed.
fn print_log_head(container: &str, lines: u64) -> Result<()> {
    let (reader, writer) = std::io::pipe().context("creating log pipe")?;
    let mut child = Command::new("docker")
        .args(["logs", container])
        .stdout(writer.try_clone()?)
        .stderr(writer)
        .spawn()
        .context("running Docker")?;
    let printed = copy_lines(
        std::io::BufReader::new(reader),
        std::io::stdout().lock(),
        lines,
    )?;
    if printed == lines {
        child.kill().context("stopping docker logs")?;
    }
    // a killed `docker logs` leaves no exit code, so only its own failures count
    if child.wait()?.code().is_some_and(|code| code != 0) {
        bail!("docker logs {container} failed");
    }
    Ok(())
}

fn copy_lines(
    mut reader: impl std::io::BufRead,
    mut out: impl std::io::Write,
    limit: u64,
) -> std::io::Result<u64> {
    let mut line = Vec::new();
    let mut copied = 0;
    while copied < limit {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        out.write_all(&line)?;
        copied += 1;
    }
    out.flush()?;
    Ok(copied)
}

fn docker_inherit(args: &[&str]) -> Result<()> {
    docker_command(args, None)
}
fn docker_inherit_in(args: &[&str], current_dir: &std::path::Path) -> Result<()> {
    docker_command(args, Some(current_dir))
}
fn docker_command(args: &[&str], current_dir: Option<&std::path::Path>) -> Result<()> {
    let mut command = docker_cli();
    command.args(args);
    if let Some(current_dir) = current_dir {
        command.current_dir(current_dir);
    }
    let status = command.status().context("running Docker")?;
    if !status.success() {
        bail!("docker {} failed", args.join(" "));
    }
    Ok(())
}
fn docker_output<const N: usize>(args: [&str; N]) -> Result<String> {
    docker_output_args(&args)
}
fn docker_output_args(args: &[&str]) -> Result<String> {
    let output = docker_cli().args(args).output().context("running Docker")?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    thread_local! {
        /// Points this test thread's Docker commands at a daemon that does not exist.
        pub(super) static DOCKER_UNREACHABLE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    fn with_unreachable_docker<T>(test: impl FnOnce() -> T) -> T {
        DOCKER_UNREACHABLE.set(true);
        let result = test();
        DOCKER_UNREACHABLE.set(false);
        result
    }

    #[test]
    fn docker_errors_are_not_reported_as_missing_resources() {
        with_unreachable_docker(|| {
            assert!(container_running("ths-alpha-app").is_err());
            assert!(
                owned_resource(&DockerCli, "volume", "ths-alpha-chain", &name("alpha")).is_err()
            );
            assert!(owned_resource(&DockerCli, "network", "ths-alpha", &name("alpha")).is_err());
            assert!(DockerEvents::default().exists("0123abcd").is_err());
        });
    }

    #[test]
    fn deleting_keeps_metadata_while_docker_is_unreachable() {
        let runtime = Runtime {
            root: std::env::temp_dir().join(format!("ths-unreachable-{}", std::process::id())),
        };
        let name = name("alpha");
        fs::create_dir_all(runtime.instance_dir(&name)).unwrap();
        runtime
            .write_instance(&name, &endpoints_for(&host_ports(0).unwrap()))
            .unwrap();
        let err = with_unreachable_docker(|| runtime.stop(&name)).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("container ths-alpha-app"),
            "container missing from {message}"
        );
        assert!(runtime.read_instance(&name).is_ok(), "metadata was deleted");
        fs::remove_dir_all(&runtime.root).unwrap();
    }

    #[test]
    fn commands_report_docker_errors_instead_of_a_stopped_environment() {
        let runtime = runtime_for_tests();
        let name = name("alpha");
        let err = with_unreachable_docker(|| runtime.mine(&name, 1, false)).unwrap_err();
        assert!(
            !err.to_string().contains("is not running"),
            "reported a Docker error as a stopped environment: {err}"
        );
    }

    use std::time::Instant;

    #[test]
    fn builds_docker_logs_arguments() {
        assert_eq!(
            logs_args("ths-default-app", false, None),
            ["logs", "ths-default-app"]
        );
        assert_eq!(
            logs_args("ths-default-zakura", true, None),
            ["logs", "--follow", "ths-default-zakura"]
        );
        assert_eq!(
            logs_args("ths-default-app", false, Some(200)),
            ["logs", "--tail", "200", "ths-default-app"]
        );
        assert_eq!(
            logs_args("ths-default-lightwalletd", true, Some(200)),
            [
                "logs",
                "--follow",
                "--tail",
                "200",
                "ths-default-lightwalletd"
            ]
        );
    }

    #[test]
    fn copies_only_the_requested_leading_lines() {
        let mut out = Vec::new();
        let mut input = std::io::Cursor::new("one\ntwo\nthree\n");
        assert_eq!(copy_lines(&mut input, &mut out, 2).unwrap(), 2);
        assert_eq!(out, b"one\ntwo\n");
        assert_eq!(input.position(), 8, "read past the requested lines");

        let mut out = Vec::new();
        let input = std::io::Cursor::new("one\ntwo");
        assert_eq!(copy_lines(input, &mut out, 5).unwrap(), 2);
        assert_eq!(out, b"one\ntwo");
    }

    #[test]
    fn balances_print_each_account_without_viewing_keys() {
        let accounts: Vec<Account> = serde_json::from_str(
            r#"[{"id":1,"name":"Account 1","unified_address":"uregtest1a","transparent_address":"tm1a","unified_full_viewing_key":"uviewregtest1a","transparent_zatoshi":50000000,"ironwood_zatoshi":500000000},
                {"id":2,"name":"Account 2","unified_address":"uregtest1b","transparent_address":"tm1b","unified_full_viewing_key":"uviewregtest1b","transparent_zatoshi":0,"ironwood_zatoshi":1}]"#,
        )
        .unwrap();
        assert_eq!(
            balance_lines(&accounts),
            "Account 1\n  Transparent  0.5 ZEC  tm1a\n  Ironwood     5 ZEC  uregtest1a\n\
             Account 2\n  Transparent  0 ZEC  tm1b\n  Ironwood     0.00000001 ZEC  uregtest1b"
        );
        assert_eq!(
            serde_json::to_value(&accounts).unwrap()[0],
            serde_json::json!({
                "id": 1,
                "transparent_address": "tm1a",
                "unified_address": "uregtest1a",
                "transparent_zatoshi": 50_000_000,
                "ironwood_zatoshi": 500_000_000,
            })
        );
    }

    #[test]
    fn balances_require_a_ready_wallet_before_reading_accounts() {
        let (url, requests) = serve(vec![
            (
                "200 OK",
                r#"{"wallet_sync":{"state":"ready","error":null}}"#,
            ),
            (
                "200 OK",
                r#"[{"id":1,"unified_address":"uregtest1a","transparent_address":"tm1a","transparent_zatoshi":50000000,"ironwood_zatoshi":500000000}]"#,
            ),
        ]);

        let accounts =
            account_balances(&url, &reqwest::blocking::Client::new(), &name("alpha")).unwrap();

        assert_eq!(accounts.len(), 1);
        assert_eq!(requests.try_iter().count(), 2);
    }

    #[test]
    fn balances_reject_syncing_and_failed_wallets_before_reading_accounts() {
        for (status, expected) in [
            (
                r#"{"wallet_sync":{"state":"syncing","error":null}}"#,
                "wallet for environment alpha is still syncing; try again later",
            ),
            (
                r#"{"wallet_sync":{"state":"error","error":"lightwalletd is unavailable"}}"#,
                "wallet synchronization failed for environment alpha: lightwalletd is unavailable",
            ),
        ] {
            let (url, requests) = serve(vec![("200 OK", status)]);

            let error = account_balances(&url, &reqwest::blocking::Client::new(), &name("alpha"))
                .unwrap_err();

            assert_eq!(error.to_string(), expected);
            assert_eq!(requests.try_iter().count(), 1);
        }
    }

    struct RecordedResource {
        name: String,
        body: String,
    }

    struct RecordingDocker {
        extra_output: BTreeMap<String, String>,
        containers: Mutex<Vec<RecordedResource>>,
        volumes: Mutex<Vec<RecordedResource>>,
        networks: Mutex<Vec<RecordedResource>>,
        runs: Mutex<Vec<String>>,
        retain: Mutex<Vec<String>>,
    }

    fn recorded_names(text: &str) -> Vec<RecordedResource> {
        text.lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(|name| RecordedResource {
                name: name.to_owned(),
                body: String::new(),
            })
            .collect()
    }

    impl RecordingDocker {
        fn new(containers: &str, volumes: &str, networks: &str) -> Self {
            Self {
                extra_output: BTreeMap::new(),
                containers: Mutex::new(recorded_names(containers)),
                volumes: Mutex::new(recorded_names(volumes)),
                networks: Mutex::new(recorded_names(networks)),
                runs: Mutex::new(Vec::new()),
                retain: Mutex::new(Vec::new()),
            }
        }

        fn inspect(&mut self, kind: &str, target: &str, body: &str) {
            let resources = match kind {
                "container" => &self.containers,
                "volume" => &self.volumes,
                _ => &self.networks,
            };
            let mut resources = resources.lock().unwrap();
            if let Some(resource) = resources
                .iter_mut()
                .find(|resource| resource.name == target)
            {
                resource.body = body.to_owned();
            } else {
                resources.push(RecordedResource {
                    name: target.to_owned(),
                    body: body.to_owned(),
                });
            }
        }
    }

    fn resource_matches(resource: &RecordedResource, token: &str) -> bool {
        resource.name == token
            || resource.body.contains(&format!("\"Id\":\"{token}\""))
            || resource.body.contains(&format!("\"Name\":\"{token}\""))
    }

    impl DockerResourceCommands for RecordingDocker {
        fn output(&self, args: &[&str]) -> Result<String> {
            if let Some(output) = self.extra_output.get(&args.join(" ")) {
                return Ok(output.clone());
            }
            let listed = |resources: &Mutex<Vec<RecordedResource>>| {
                resources
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|resource| resource.name.clone())
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            match args {
                ["container", "ls", "-a", "--format", "{{.Names}}"] => Ok(listed(&self.containers)),
                ["volume", "ls", "--format", "{{.Name}}"] => Ok(listed(&self.volumes)),
                ["network", "ls", "--format", "{{.Name}}"] => Ok(listed(&self.networks)),
                [kind, "inspect", target] => {
                    let resources = match *kind {
                        "container" => &self.containers,
                        "volume" => &self.volumes,
                        "network" => &self.networks,
                        _ => bail!("unexpected Docker read: {}", args.join(" ")),
                    };
                    resources
                        .lock()
                        .unwrap()
                        .iter()
                        .find(|resource| {
                            resource.name == *target || resource_matches(resource, target)
                        })
                        .map(|resource| resource.body.clone())
                        .filter(|body| !body.is_empty())
                        .ok_or_else(|| anyhow!("unexpected Docker read: {}", args.join(" ")))
                }
                _ => bail!("unexpected Docker read: {}", args.join(" ")),
            }
        }

        fn run(&self, args: &[&str]) -> Result<()> {
            self.runs.lock().unwrap().push(args.join(" "));
            let token = args.last().copied().unwrap_or("");
            if self.retain.lock().unwrap().iter().any(|kept| kept == token) {
                return Ok(());
            }
            let resources = match args {
                ["rm", "-f", _] => &self.containers,
                ["volume", "rm", _] => &self.volumes,
                ["network", "rm", _] => &self.networks,
                _ => return Ok(()),
            };
            resources
                .lock()
                .unwrap()
                .retain(|resource| !resource_matches(resource, token));
            Ok(())
        }
    }

    fn allocate_replaced_app(docker: &ReplacedApp) -> Result<WatchedContainer> {
        let dir = tempfile::tempdir()?;
        let mut journal = recovery::RecoveryJournal::create(
            dir.path().join("lifecycle-recovery.json"),
            &name("alpha"),
        )?;
        let volume = journal.begin(
            recovery::ResourceRef {
                kind: recovery::ResourceKind::Volume,
                name: "ths-alpha-wallet".into(),
                identity: Some("ths-alpha-wallet".into()),
            },
            Vec::new(),
            recovery::MutationOperation::Create,
        )?;
        journal.finish(
            volume,
            recovery::MutationOutcome::Acknowledged,
            Some("ths-alpha-wallet".into()),
        )?;
        ensure_app(
            "ths-alpha",
            &name("alpha"),
            &host_ports(0)?,
            docker,
            &mut journal,
            Deadline::after(Duration::from_secs(5)),
            Vec::new(),
        )
    }

    struct ReplacedApp {
        reuse: bool,
        inspect_error: bool,
    }

    impl DockerResourceCommands for ReplacedApp {
        fn output(&self, args: &[&str]) -> Result<String> {
            match args {
                ["container", "ls", "-a", "--format", "{{.Names}}"] => {
                    Ok(if self.reuse { "ths-alpha-app" } else { "" }.into())
                }
                ["container", "inspect", "ths-alpha-app" | "app-1"] => Ok(
                    r#"[{"Id":"app-1","Config":{"Labels":{"com.zakura.ths.instance":"alpha"}}}]"#
                        .into(),
                ),
                ["create", ..] => Ok("app-1".into()),
                ["container", "inspect", "--format", "{{.Created}}", "app-1"] => {
                    if self.inspect_error {
                        bail!("Docker is unreachable");
                    }
                    Ok("2026-01-01T00:00:00Z".into())
                }
                // After creation (or ownership inspection), the name points to a replacement.
                [
                    "container",
                    "inspect",
                    "--format",
                    "{{.Created}}",
                    "ths-alpha-app",
                ] => Ok("2026-01-02T00:00:00Z".into()),
                _ => bail!("unexpected Docker read: {}", args.join(" ")),
            }
        }

        fn run(&self, args: &[&str]) -> Result<()> {
            bail!("unexpected Docker mutation: {}", args.join(" "))
        }
    }

    #[test]
    fn app_allocation_captures_identity_before_a_name_replacement() {
        for reuse in [false, true] {
            let docker = ReplacedApp {
                reuse,
                inspect_error: false,
            };
            let app = allocate_replaced_app(&docker).unwrap();
            assert_eq!(app, watched("app-1"));
        }
    }

    #[test]
    fn app_identity_capture_does_not_fall_back_to_a_replacement_on_docker_errors() {
        let docker = ReplacedApp {
            reuse: false,
            inspect_error: true,
        };
        let error = allocate_replaced_app(&docker).unwrap_err();
        assert!(error.to_string().contains("Docker is unreachable"));
    }

    #[test]
    fn foreign_volume_collision_preserves_all_resources() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = Runtime {
            root: dir.path().to_path_buf(),
        };
        let mut docker = RecordingDocker::new("ths-alpha-app", "ths-alpha-wallet", "");
        docker.inspect(
            "container",
            "ths-alpha-app",
            r#"[{"Id":"owned-container","Config":{"Labels":{"com.zakura.ths.instance":"alpha"}}}]"#,
        );
        docker.inspect(
            "volume",
            "ths-alpha-wallet",
            r#"[{"Name":"ths-alpha-wallet","Labels":{"com.zakura.ths.instance":"other"}}]"#,
        );

        let error = runtime
            .delete_instance_resources_with(&name("alpha"), &docker)
            .unwrap_err();

        assert!(format!("{error:#}").contains("not owned"), "{error:#}");
        assert!(docker.runs.lock().unwrap().is_empty());
    }

    #[test]
    fn partial_startup_cleanup_removes_only_owned_resources() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = Runtime {
            root: dir.path().to_path_buf(),
        };
        let metadata = runtime.instance_dir(&name("alpha"));
        fs::create_dir_all(&metadata).unwrap();
        fs::write(metadata.join("instance.json"), "partial startup").unwrap();
        let mut docker = RecordingDocker::new("", "ths-alpha-chain\nths-alpha-wallet", "ths-alpha");
        docker.inspect(
            "volume",
            "ths-alpha-chain",
            r#"[{"Name":"ths-alpha-chain","Labels":{"com.zakura.ths.instance":"alpha"}}]"#,
        );
        docker.inspect(
            "volume",
            "ths-alpha-wallet",
            r#"[{"Name":"ths-alpha-wallet","Labels":{"com.zakura.ths.instance":"other"}}]"#,
        );
        docker.inspect(
            "network",
            "ths-alpha",
            r#"[{"Id":"owned-network","Labels":{"com.zakura.ths.instance":"alpha"}}]"#,
        );

        let error = runtime
            .delete_instance_resources_with_mode(&name("alpha"), &docker, true)
            .unwrap_err();

        assert!(format!("{error:#}").contains("not owned"), "{error:#}");
        assert_eq!(
            *docker.runs.lock().unwrap(),
            ["volume rm ths-alpha-chain", "network rm owned-network"]
        );
        assert_eq!(
            fs::read_to_string(metadata.join("instance.json")).unwrap(),
            "partial startup"
        );
    }

    #[test]
    fn foreign_container_collision_is_not_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = Runtime {
            root: dir.path().to_path_buf(),
        };
        let mut docker = RecordingDocker::new("ths-alpha-app", "", "");
        docker.inspect(
            "container",
            "ths-alpha-app",
            r#"[{"Id":"foreign-container","Config":{"Labels":{"com.zakura.ths.instance":"other"}}}]"#,
        );

        let error = runtime
            .delete_instance_resources_with(&name("alpha"), &docker)
            .unwrap_err();

        assert!(format!("{error:#}").contains("not owned"), "{error:#}");
        assert!(docker.runs.lock().unwrap().is_empty());
    }

    #[test]
    fn owned_resources_still_clean_up() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = Runtime {
            root: dir.path().to_path_buf(),
        };
        let mut docker = RecordingDocker::new("ths-alpha-app", "ths-alpha-wallet", "ths-alpha");
        docker.inspect(
            "container",
            "ths-alpha-app",
            r#"[{"Id":"owned-container","Config":{"Labels":{"com.zakura.ths.instance":"alpha"}}}]"#,
        );
        docker.inspect(
            "volume",
            "ths-alpha-wallet",
            r#"[{"Name":"ths-alpha-wallet","Labels":{"com.zakura.ths.instance":"alpha"}}]"#,
        );
        docker.inspect(
            "network",
            "ths-alpha",
            r#"[{"Id":"owned-network","Labels":{"com.zakura.ths.instance":"alpha"}}]"#,
        );

        runtime
            .delete_instance_resources_with(&name("alpha"), &docker)
            .unwrap();

        assert_eq!(
            *docker.runs.lock().unwrap(),
            [
                "rm -f owned-container",
                "volume rm ths-alpha-wallet",
                "network rm owned-network",
            ]
        );
    }

    #[test]
    fn unlabeled_network_collision_preserves_data_and_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = Runtime {
            root: dir.path().to_path_buf(),
        };
        let metadata = runtime.instance_dir(&name("alpha"));
        fs::create_dir_all(&metadata).unwrap();
        fs::write(metadata.join("instance.json"), "important data").unwrap();
        let mut docker = RecordingDocker::new("ths-alpha-app", "ths-alpha-wallet", "ths-alpha");
        docker.inspect(
            "container",
            "ths-alpha-app",
            r#"[{"Id":"owned-container","Config":{"Labels":{"com.zakura.ths.instance":"alpha"}}}]"#,
        );
        docker.inspect(
            "volume",
            "ths-alpha-wallet",
            r#"[{"Name":"ths-alpha-wallet","Labels":{"com.zakura.ths.instance":"alpha"}}]"#,
        );
        docker.inspect(
            "network",
            "ths-alpha",
            r#"[{"Id":"old-network","Labels":{}}]"#,
        );

        let error = runtime
            .delete_instance_resources_with(&name("alpha"), &docker)
            .unwrap_err();

        assert!(format!("{error:#}").contains("not owned"), "{error:#}");
        assert!(docker.runs.lock().unwrap().is_empty());
        assert_eq!(
            fs::read_to_string(metadata.join("instance.json")).unwrap(),
            "important data"
        );
    }

    #[test]
    fn new_networks_are_labeled_for_owned_cleanup() {
        struct NewNetwork(AtomicBool);

        impl DockerResourceCommands for NewNetwork {
            fn output(&self, args: &[&str]) -> Result<String> {
                match args {
                    ["network", "ls", "--format", "{{.Name}}"] => {
                        Ok(if self.0.load(Ordering::SeqCst) {
                            "ths-alpha".into()
                        } else {
                            String::new()
                        })
                    }
                    ["network", "inspect", "ths-alpha"] => Ok(
                        r#"[{"Id":"owned-network","Labels":{"com.zakura.ths.instance":"alpha"}}]"#
                            .into(),
                    ),
                    _ => bail!("unexpected Docker read: {args:?}"),
                }
            }

            fn run(&self, args: &[&str]) -> Result<()> {
                assert_eq!(
                    args,
                    [
                        "network",
                        "create",
                        "--label",
                        "com.zakura.ths.instance=alpha",
                        "ths-alpha"
                    ]
                );
                self.0.store(true, Ordering::SeqCst);
                Ok(())
            }
        }

        let docker = NewNetwork(AtomicBool::new(false));

        let dir = tempfile::tempdir().unwrap();
        let mut journal = recovery::RecoveryJournal::create(
            dir.path().join(recovery::RECOVERY_FILE),
            &name("alpha"),
        )
        .unwrap();
        ensure_network_with(
            "ths-alpha",
            &name("alpha"),
            &docker,
            &mut journal,
            Deadline::after(Duration::from_secs(1)),
        )
        .unwrap();
        assert!(docker.0.load(Ordering::SeqCst));
    }

    #[test]
    fn duplicate_network_names_are_rejected() {
        let docker = RecordingDocker::new("", "", "ths-alpha\nths-alpha");

        let dir = tempfile::tempdir().unwrap();
        let mut journal = recovery::RecoveryJournal::create(
            dir.path().join(recovery::RECOVERY_FILE),
            &name("alpha"),
        )
        .unwrap();
        let error = ensure_network_with(
            "ths-alpha",
            &name("alpha"),
            &docker,
            &mut journal,
            Deadline::after(Duration::from_secs(1)),
        )
        .unwrap_err();

        assert!(
            error.to_string().contains("multiple network resources"),
            "{error:#}"
        );
        assert!(docker.runs.lock().unwrap().is_empty());
    }

    #[test]
    fn volume_create_does_not_accept_a_racing_foreign_volume() {
        struct RacingVolume(AtomicBool);

        impl DockerResourceCommands for RacingVolume {
            fn output(&self, args: &[&str]) -> Result<String> {
                match args {
                    ["volume", "ls", "--format", "{{.Name}}"] => Ok(if self.0.load(Ordering::SeqCst) {
                        "ths-alpha-wallet".into()
                    } else {
                        String::new()
                    }),
                    ["volume", "inspect", "ths-alpha-wallet"] => Ok(
                        r#"[{"Name":"ths-alpha-wallet","Labels":{"com.zakura.ths.instance":"other"}}]"#.into(),
                    ),
                    _ => bail!("unexpected Docker read: {args:?}"),
                }
            }

            fn run(&self, args: &[&str]) -> Result<()> {
                assert_eq!(
                    args,
                    [
                        "volume",
                        "create",
                        "--label",
                        "com.zakura.ths.instance=alpha",
                        "ths-alpha-wallet"
                    ]
                );
                self.0.store(true, Ordering::SeqCst);
                Ok(())
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let mut journal = recovery::RecoveryJournal::create(
            dir.path().join(recovery::RECOVERY_FILE),
            &name("alpha"),
        )
        .unwrap();
        let error = ensure_volume_with(
            "ths-alpha-wallet",
            &name("alpha"),
            &RacingVolume(AtomicBool::new(false)),
            &mut journal,
            Deadline::after(Duration::from_secs(1)),
        )
        .unwrap_err();

        assert!(error.to_string().contains("not owned"), "{error:#}");
    }

    fn environment(name: &str, states: [&str; 4]) -> EnvironmentStatus {
        let containers = SERVICES
            .into_iter()
            .zip(states)
            .map(|(service, state)| ContainerStatus {
                service,
                state: state.into(),
                created_at: (state != "missing").then_some(1_000),
                started_at: (state == "running" || state == "exited").then_some(1_060),
            })
            .collect::<Vec<_>>();
        EnvironmentStatus {
            instance: Instance {
                name: name.into(),
                version: 1,
                endpoints: endpoints_for(&HostPorts {
                    dashboard: 8080,
                    rpc: 18232,
                    lightwalletd: 9067,
                    p2p: 18233,
                }),
                created_at: Some(1_000),
            },
            state: environment_state(&containers),
            containers,
        }
    }

    #[test]
    fn environments_are_classified_by_their_long_running_services() {
        let state = |states| environment("e", states).state;
        assert_eq!(
            state(["running", "running", "running", "exited"]),
            "running"
        );
        assert_eq!(
            state(["exited", "running", "running", "exited"]),
            "degraded"
        );
        assert_eq!(
            state(["missing", "running", "missing", "missing"]),
            "degraded"
        );
        assert_eq!(state(["exited", "exited", "exited", "exited"]), "stopped");
        assert_eq!(
            state(["missing", "missing", "missing", "missing"]),
            "stopped"
        );
        // init is one-shot: running or not, it does not decide the state.
        assert_eq!(state(["exited", "exited", "exited", "running"]), "stopped");
        // a paused service is still up.
        assert_eq!(state(["paused", "paused", "paused", "exited"]), "paused");
        assert_eq!(state(["paused", "running", "running", "exited"]), "paused");
        assert_eq!(state(["paused", "exited", "running", "exited"]), "degraded");
        // any container under its names that it doesn't own, init included.
        assert_eq!(
            state(["foreign", "foreign", "foreign", "missing"]),
            "conflict"
        );
        assert_eq!(
            state(["running", "running", "running", "foreign"]),
            "conflict"
        );
    }

    #[test]
    fn the_list_groups_active_before_inactive_and_hints_only_for_inactive() {
        let active = environment("default", ["running", "running", "running", "exited"]);
        let degraded = environment("partial", ["exited", "running", "running", "exited"]);
        let stopped = environment("old", ["missing", "missing", "missing", "missing"]);
        let rendered = render_list(&[stopped, active, degraded], 1_000 + 7_200);
        assert_eq!(
            rendered,
            concat!(
                "ACTIVE\n",
                "default          http://127.0.0.1:8080      running    created 2h ago\n",
                "  app           running   created 2h ago    started 1h ago\n",
                "  zakura        running   created 2h ago    started 1h ago\n",
                "  lightwalletd  running   created 2h ago    started 1h ago\n",
                "  init          exited    created 2h ago    started 1h ago\n",
                "partial          http://127.0.0.1:8080      degraded   created 2h ago\n",
                "  app           exited    created 2h ago    started 1h ago\n",
                "  zakura        running   created 2h ago    started 1h ago\n",
                "  lightwalletd  running   created 2h ago    started 1h ago\n",
                "  init          exited    created 2h ago    started 1h ago\n",
                "\n",
                "INACTIVE\n",
                "old              http://127.0.0.1:8080      stopped    created 2h ago\n",
                "  app           missing\n",
                "  zakura        missing\n",
                "  lightwalletd  missing\n",
                "  init          missing\n",
                "\n",
                "Remove an inactive environment and all of its data with:\n",
                "  ths --name <name> stop\n",
            )
        );
        // no inactive environment: no empty section and no hint.
        let only_active = render_list(
            &[environment(
                "default",
                ["running", "running", "running", "exited"],
            )],
            1_000,
        );
        assert!(!only_active.contains("INACTIVE") && !only_active.contains("ths --name"));
        let only_inactive = render_list(
            &[environment("old", ["exited", "exited", "exited", "exited"])],
            1_000,
        );
        assert!(only_inactive.starts_with("INACTIVE\n") && only_inactive.contains("ths --name"));
    }

    #[test]
    fn the_json_list_keeps_its_fields_and_adds_absolute_status() {
        let json = serde_json::to_value(environment(
            "default",
            ["running", "exited", "missing", "exited"],
        ))
        .unwrap();
        assert_eq!(json["name"], "default");
        assert_eq!(json["version"], 1);
        assert_eq!(json["endpoints"]["dashboard"], "http://127.0.0.1:8080");
        assert_eq!(json["state"], "degraded");
        assert_eq!(json["created_at"], 1_000);
        assert_eq!(json["containers"][0]["service"], "app");
        assert_eq!(json["containers"][0]["started_at"], 1_060);
        assert_eq!(json["containers"][2]["state"], "missing");
        assert!(json["containers"][2]["created_at"].is_null());
    }

    #[test]
    fn environments_recorded_before_creation_times_still_list() {
        let mut old = environment("old", ["missing", "missing", "missing", "missing"]);
        let mut legacy = serde_json::to_value(&old.instance).unwrap();
        legacy.as_object_mut().unwrap().remove("created_at");
        old.instance = serde_json::from_value(legacy).unwrap();
        assert_eq!(old.instance.created_at, None);
        assert!(
            render_list(&[old], 1_000)
                .starts_with("INACTIVE\nold              http://127.0.0.1:8080      stopped\n")
        );
    }

    #[test]
    fn environments_still_list_when_docker_is_unavailable() {
        let saved = environment("default", ["running", "running", "running", "exited"]).instance;
        let unknown =
            environment_status(saved, Err(anyhow!("Cannot connect to the Docker daemon")));
        let json = serde_json::to_value(&unknown).unwrap();
        assert_eq!(json["name"], "default");
        assert_eq!(json["endpoints"]["dashboard"], "http://127.0.0.1:8080");
        assert_eq!(json["created_at"], 1_000);
        assert_eq!(json["state"], "unknown");
        assert_eq!(json["containers"], serde_json::json!([]));
        assert_eq!(
            render_list(&[unknown], 1_000 + 7_200),
            "UNKNOWN\ndefault          http://127.0.0.1:8080      unknown    created 2h ago\n"
        );
    }

    /// docker holding alpha's service containers, each with an optional owner label and a status.
    fn docker_holding(containers: &[(&str, Option<&str>, &str)]) -> RecordingDocker {
        let names: Vec<_> = containers
            .iter()
            .map(|(service, ..)| format!("ths-alpha-{service}"))
            .collect();
        let mut docker = RecordingDocker::new(&names.join("\n"), "", "");
        for (service, owner, status) in containers {
            let labels = owner.map_or("{}".to_owned(), |owner| {
                format!(r#"{{"{INSTANCE_LABEL}":"{owner}"}}"#)
            });
            docker.inspect(
                "container",
                &format!("ths-alpha-{service}"),
                &format!(r#"[{{"Id":"{service}-id","Config":{{"Labels":{labels}}}}}]"#),
            );
            docker.extra_output.insert(
                format!(
                    "container inspect --format {{{{.State.Status}}}} {{{{.Created}}}} {{{{.State.StartedAt}}}} {service}-id"
                ),
                format!("{status} 2026-10-09T10:00:00Z 2026-10-09T10:01:00Z"),
            );
        }
        docker
    }

    #[test]
    fn listing_checks_container_ownership_and_reports_paused_stacks() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = Runtime {
            root: dir.path().to_path_buf(),
        };
        fs::create_dir_all(runtime.instance_dir(&name("alpha"))).unwrap();
        runtime
            .write_instance(&name("alpha"), &endpoints_for(&host_ports(0).unwrap()))
            .unwrap();
        let listed = |docker: &RecordingDocker| {
            let environments = runtime.environments(docker).unwrap();
            let containers: Vec<_> = environments[0]
                .containers
                .iter()
                .map(|c| c.state.clone())
                .collect();
            let rendered = render_list(&environments, now_unix().unwrap());
            (environments[0].state, containers, rendered)
        };

        // stale metadata over unrelated, unlabeled containers holding the service names.
        let (state, containers, rendered) = listed(&docker_holding(&[
            ("app", None, "running"),
            ("zakura", None, "running"),
            ("lightwalletd", None, "running"),
        ]));
        assert_eq!(state, "conflict");
        assert_eq!(containers, ["foreign", "foreign", "foreign", "missing"]);
        assert!(
            rendered.starts_with("CONFLICT\nalpha") && !rendered.contains("ths --name"),
            "{rendered}"
        );
        // a container another instance owns is foreign too.
        let (state, ..) = listed(&docker_holding(&[
            ("app", Some("other"), "running"),
            ("zakura", Some("alpha"), "running"),
            ("lightwalletd", Some("alpha"), "running"),
        ]));
        assert_eq!(state, "conflict");

        for (statuses, expected) in [
            (["running", "running", "running"], "running"),
            (["paused", "paused", "paused"], "paused"),
            (["paused", "running", "running"], "paused"),
        ] {
            let (state, containers, rendered) = listed(&docker_holding(&[
                ("app", Some("alpha"), statuses[0]),
                ("zakura", Some("alpha"), statuses[1]),
                ("lightwalletd", Some("alpha"), statuses[2]),
            ]));
            assert_eq!(state, expected);
            assert_eq!(containers[..3], statuses);
            assert!(
                rendered.starts_with("ACTIVE\nalpha") && !rendered.contains("ths --name"),
                "{rendered}"
            );
        }
    }

    #[test]
    fn docker_timestamps_parse_to_unix_seconds() {
        assert_eq!(parse_docker_time("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_docker_time("2026-10-02T21:58:53.061174304Z"),
            Some(1_790_978_333)
        );
        assert_eq!(
            parse_docker_time("2024-02-29T12:00:00Z"),
            Some(1_709_208_000)
        );
        // docker's zero time for a container that never started.
        assert_eq!(parse_docker_time("0001-01-01T00:00:00Z"), None);
        assert_eq!(parse_docker_time("not a time"), None);
    }

    struct RecordingHost {
        events: Arc<Mutex<Vec<String>>>,
        initial_delete_error: bool,
        shutdown_collision: Option<RecordingDocker>,
        wait_ready_result: Result<(), String>,
        open_url_result: Result<(), String>,
        interrupt_before_ready: bool,
        final_cleanup_failure: Option<String>,
        deleted_elsewhere: bool,
        allocate_delay: Duration,
        run_status: Option<RuntimeStatus>,
    }

    impl RecordingHost {
        fn new() -> (Self, Arc<Mutex<Vec<String>>>) {
            let events = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    events: events.clone(),
                    initial_delete_error: false,
                    shutdown_collision: None,
                    wait_ready_result: Ok(()),
                    open_url_result: Ok(()),
                    interrupt_before_ready: false,
                    final_cleanup_failure: None,
                    deleted_elsewhere: false,
                    allocate_delay: Duration::ZERO,
                    run_status: None,
                },
                events,
            )
        }

        fn push(&self, event: &str) {
            self.events.lock().unwrap().push(event.to_owned());
        }
    }

    fn verified_report() -> CleanupReport {
        CleanupReport {
            outcome: CleanupOutcome::VerifiedComplete,
            failures: Vec::new(),
            recovery_path: None,
            helpers_finished: true,
        }
    }

    impl StartHost for RecordingHost {
        fn runtime_status(
            &self,
            _endpoints: &Endpoints,
            _timeout: Duration,
        ) -> Result<RuntimeStatus> {
            self.push("runtime_status");
            self.run_status
                .clone()
                .context("no recorded runtime status")
        }

        fn run_child(
            &self,
            _command: &[OsString],
            _current_dir: &Path,
            name: &InstanceName,
            _endpoints: &Endpoints,
            _shutdown: &Shutdown,
        ) -> Result<ChildOutcome> {
            self.push(&format!("child:{name}"));
            Ok(ChildOutcome::Exited(42))
        }

        fn delete(
            &self,
            runtime: &Runtime,
            name: &InstanceName,
            context: &LifecycleContext,
            _deadline: Deadline,
        ) -> Result<CleanupReport> {
            let later =
                self.events.lock().unwrap().iter().any(|event| {
                    event.starts_with("delete:") || event.starts_with("delete_partial:")
                });
            self.push(&format!("delete:{name}"));
            if later {
                self.push(&format!("final_cleanup:{name}"));
            }
            if !later && self.initial_delete_error {
                bail!("resource collision");
            }
            if later
                && self
                    .events
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|event| event.starts_with("wait_for_shutdown"))
                && let Some(docker) = &self.shutdown_collision
            {
                return runtime.cleanup_with(name, docker, false, &context.policy);
            }
            if (later || self.run_status.is_some())
                && let Some(message) = &self.final_cleanup_failure
            {
                return Ok(CleanupReport {
                    outcome: CleanupOutcome::Uncertain,
                    failures: vec![message.clone()],
                    recovery_path: Some(runtime.recovery_path(name)),
                    helpers_finished: true,
                });
            }
            Ok(verified_report())
        }

        fn delete_partial(
            &self,
            runtime: &Runtime,
            name: &InstanceName,
            context: &LifecycleContext,
            deadline: Deadline,
        ) -> Result<CleanupReport> {
            if let Some(docker) = &self.shutdown_collision {
                let later = self.events.lock().unwrap().iter().any(|event| {
                    event.starts_with("delete:") || event.starts_with("delete_partial:")
                });
                self.push(&format!("delete_partial:{name}"));
                if later {
                    self.push(&format!("final_cleanup:{name}"));
                }
                runtime.cleanup_with(name, docker, true, &context.policy)
            } else {
                self.delete(runtime, name, context, deadline)
            }
        }

        fn allocate(
            &self,
            _runtime: &Runtime,
            name: &InstanceName,
            shutdown: &Shutdown,
            _port_offset: u16,
            _context: &LifecycleContext,
            _journal: &mut recovery::RecoveryJournal,
        ) -> Result<AllocatedInstance> {
            self.push(&format!("allocate:{name}"));
            std::thread::sleep(self.allocate_delay);
            shutdown.check()?;
            Ok(AllocatedInstance {
                endpoints: Endpoints {
                    dashboard: "http://127.0.0.1:1".into(),
                    rpc: "http://127.0.0.1:2".into(),
                    lightwalletd: "http://127.0.0.1:3".into(),
                    p2p: "127.0.0.1:4".into(),
                    network: default_regtest(),
                    tls: false,
                },
                resources: Vec::new(),
                app_container: watched("app-1"),
                zakura_container_id: "zakura-id".into(),
            })
        }

        fn wait_ready(
            &self,
            _allocated: &AllocatedInstance,
            shutdown: &Shutdown,
            _context: &LifecycleContext,
        ) -> Result<()> {
            self.push("wait_ready");
            if self.interrupt_before_ready {
                bail!("interrupted");
            }
            shutdown.check()?;
            self.wait_ready_result
                .as_ref()
                .map(|_| ())
                .map_err(|e| anyhow!("{e}"))
        }

        fn open_url(
            &self,
            url: &str,
            _shutdown: &Shutdown,
            _context: &LifecycleContext,
        ) -> Result<()> {
            self.push(&format!("open_url:{url}"));
            self.open_url_result
                .as_ref()
                .map(|_| ())
                .map_err(|e| anyhow!("{e}"))
        }

        fn wait_for_shutdown(
            &self,
            app_container: &WatchedContainer,
            shutdown: &Shutdown,
        ) -> Result<Ended> {
            self.push(&format!("wait_for_shutdown:{}", app_container.id));
            assert_eq!(app_container, &watched("app-1"));
            if self.shutdown_collision.is_some() {
                return Ok(Ended::Interrupted);
            }
            let events = ScriptedEvents::default();
            if self.deleted_elsewhere {
                events.streams.lock().unwrap().push_back(Ok(true));
            }
            let _monitor = DeletionMonitor::spawn(
                Arc::new(events),
                app_container,
                Duration::from_millis(1),
                shutdown,
            );
            shutdown.wait()
        }
    }

    fn runtime_for_tests() -> Runtime {
        let root = std::env::temp_dir().join(format!("ths-start-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        Runtime { root }
    }

    fn name(value: &str) -> InstanceName {
        value.parse().unwrap()
    }

    #[test]
    fn run_timeout_after_allocation_skips_child_and_cleans_its_instance() {
        let (mut host, events) = RecordingHost::new();
        host.allocate_delay = Duration::from_millis(80);
        let mut context = LifecycleContext::new(LifecyclePolicy::default());
        context.overall_deadline = Some(Deadline::after(Duration::from_millis(30)));
        let (_sender, shutdown) = Shutdown::channel();
        let runtime = runtime_for_tests();
        let error = runtime
            .run_with_policy(
                &name("alpha"),
                &[OsString::from("true")],
                &runtime.root,
                &host,
                &shutdown,
                &context,
            )
            .unwrap_err();
        assert!(format!("{error:#}").contains("timeout"), "{error:#}");
        let events = events.lock().unwrap();
        assert!(events.contains(&"allocate:alpha".to_owned()));
        assert!(events.contains(&"delete:alpha".to_owned()));
        assert!(
            !events
                .iter()
                .any(|event| event == "wait_ready" || event.starts_with("child:"))
        );
    }

    #[test]
    fn settlement_does_not_accept_a_late_status_response() {
        let (_sender, shutdown) = Shutdown::channel();
        let started = Instant::now();
        let error = wait_until_settled(
            &|_| {
                std::thread::sleep(Duration::from_millis(80));
                Ok(RuntimeStatus {
                    node_height: Some(1),
                    lightwalletd_height: Some(1),
                    wallet_scanned_height: Some(1),
                    wallet_sync_state: "ready".into(),
                    settled: true,
                    settlement_blocker: None,
                })
            },
            started + Duration::from_millis(30),
            Duration::from_millis(750),
            &shutdown,
            false,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("timeout"), "{error:#}");
        assert!(started.elapsed() < Duration::from_millis(250));
    }

    #[test]
    fn run_preserves_child_exit_code_and_cleans_after_settlement() {
        let (mut host, events) = RecordingHost::new();
        host.run_status = Some(RuntimeStatus {
            node_height: Some(105),
            lightwalletd_height: Some(105),
            wallet_scanned_height: Some(105),
            wallet_sync_state: "ready".into(),
            settled: true,
            settlement_blocker: None,
        });
        let mut context = LifecycleContext::new(LifecyclePolicy::default());
        context.overall_deadline = Some(Deadline::after(Duration::from_secs(1)));
        let (_sender, shutdown) = Shutdown::channel();
        let runtime = runtime_for_tests();
        let code = runtime
            .run_with_policy(
                &name("alpha"),
                &[OsString::from("true")],
                &runtime.root,
                &host,
                &shutdown,
                &context,
            )
            .unwrap();
        assert_eq!(code, 42);
        let events = events.lock().unwrap();
        assert!(events.contains(&"runtime_status".to_owned()));
        assert!(events.contains(&"child:alpha".to_owned()));
        assert_eq!(events.last().map(String::as_str), Some("delete:alpha"));
    }

    #[test]
    fn failed_cleanup_reports_error_without_replacing_child_exit_code() {
        let (mut host, _events) = RecordingHost::new();
        host.run_status = Some(RuntimeStatus {
            node_height: Some(105),
            lightwalletd_height: Some(105),
            wallet_scanned_height: Some(105),
            wallet_sync_state: "ready".into(),
            settled: true,
            settlement_blocker: None,
        });
        host.final_cleanup_failure = Some("Docker cleanup failed".into());
        let mut context = LifecycleContext::new(LifecyclePolicy::default());
        context.overall_deadline = Some(Deadline::after(Duration::from_secs(1)));
        let (_sender, shutdown) = Shutdown::channel();
        let runtime = runtime_for_tests();
        let code = runtime
            .run_with_policy(
                &name("alpha"),
                &[OsString::from("true")],
                &runtime.root,
                &host,
                &shutdown,
                &context,
            )
            .unwrap();
        assert_eq!(code, 42);
    }

    #[test]
    fn run_refuses_existing_instance_without_cleanup() {
        let (host, events) = RecordingHost::new();
        let mut context = LifecycleContext::new(LifecyclePolicy::default());
        context.overall_deadline = Some(Deadline::after(Duration::from_secs(1)));
        let (_sender, shutdown) = Shutdown::channel();
        let runtime = runtime_for_tests();
        fs::create_dir_all(runtime.instance_dir(&name("alpha"))).unwrap();
        let error = runtime
            .run_with_policy(
                &name("alpha"),
                &[OsString::from("true")],
                &runtime.root,
                &host,
                &shutdown,
                &context,
            )
            .unwrap_err();
        assert!(format!("{error:#}").contains("already exists"));
        assert!(events.lock().unwrap().is_empty());
    }

    #[test]
    fn stop_does_not_delete_an_instance_held_by_run() {
        let runtime = runtime_for_tests();
        let lock = runtime.lock_instance(&name("alpha")).unwrap();
        let error = runtime.stop(&name("alpha")).unwrap_err();
        assert!(format!("{error:#}").contains("already in use"));
        drop(lock);
    }

    #[test]
    fn child_receives_project_directory_and_regtest_endpoints() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("child-environment");
        let command = vec![
            OsString::from("sh"),
            OsString::from("-c"),
            OsString::from("{ pwd; env; } > \"$1\""),
            OsString::from("sh"),
            output.as_os_str().to_owned(),
        ];
        let (_sender, shutdown) = Shutdown::channel();
        let outcome = run_child(
            &command,
            dir.path(),
            &name("alpha"),
            &endpoints_for(&host_ports(0).unwrap()),
            &shutdown,
            Duration::from_millis(50),
        )
        .unwrap();
        assert_eq!(outcome, ChildOutcome::Exited(0));
        let output = fs::read_to_string(output).unwrap();
        assert_eq!(
            output.lines().next(),
            fs::canonicalize(dir.path()).unwrap().to_str()
        );
        for expected in [
            "THS_INSTANCE=alpha",
            "THS_NETWORK=regtest",
            "THS_API_URL=http://127.0.0.1:32805",
            "THS_ZAKURA_RPC_URL=http://127.0.0.1:18232",
            "THS_LIGHTWALLETD_URL=http://127.0.0.1:9067",
            "THS_LIGHTWALLETD_TLS=false",
            "THS_P2P_ADDR=127.0.0.1:18233",
        ] {
            assert!(
                output.lines().any(|line| line == expected),
                "missing {expected}"
            );
        }
        assert!(!output.lines().any(|line| line.starts_with("THS_SEED=")));
    }

    #[test]
    fn child_exit_code_is_preserved() {
        let command = [
            OsString::from("sh"),
            OsString::from("-c"),
            OsString::from("exit 7"),
        ];
        let (_sender, shutdown) = Shutdown::channel();
        let outcome = run_child(
            &command,
            Path::new("."),
            &name("alpha"),
            &endpoints_for(&host_ports(0).unwrap()),
            &shutdown,
            Duration::from_millis(50),
        )
        .unwrap();
        assert_eq!(outcome, ChildOutcome::Exited(7));
    }

    #[cfg(unix)]
    #[test]
    fn interrupted_child_terminates_its_background_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("descendant.pid");
        let command = vec![
            OsString::from("sh"),
            OsString::from("-c"),
            OsString::from("sleep 30 & echo $! > \"$1\"; wait"),
            OsString::from("sh"),
            pid_file.as_os_str().to_owned(),
        ];
        let (sender, shutdown) = Shutdown::channel();
        let signal_file = pid_file.clone();
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !signal_file.exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            sender.send(Ended::Interrupted).unwrap();
        });
        let outcome = run_child(
            &command,
            dir.path(),
            &name("alpha"),
            &endpoints_for(&host_ports(0).unwrap()),
            &shutdown,
            Duration::from_millis(50),
        )
        .unwrap();
        assert_eq!(outcome, ChildOutcome::Signaled);
        let pid = fs::read_to_string(pid_file).unwrap();
        let alive = Command::new("kill")
            .args(["-0", pid.trim()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        assert!(!alive, "background descendant {} survived", pid.trim());
    }

    #[test]
    fn initial_cleanup_failure_does_not_retry_on_drop() {
        let (mut host, events) = RecordingHost::new();
        host.initial_delete_error = true;
        let (_sender, shutdown) = Shutdown::channel();

        let error = runtime_for_tests()
            .start_with(&name("alpha"), false, false, 0, &host, &shutdown)
            .unwrap_err();

        assert!(error.to_string().contains("resource collision"));
        assert_eq!(*events.lock().unwrap(), ["delete:alpha"]);
    }

    #[test]
    fn shutdown_collision_removes_owned_resources_and_preserves_foreign_container() {
        let mut docker = RecordingDocker::new(
            "ths-alpha-app\nths-alpha-init",
            "ths-alpha-wallet",
            "ths-alpha",
        );
        docker.inspect(
            "container",
            "ths-alpha-app",
            r#"[{"Id":"owned-app","Config":{"Labels":{"com.zakura.ths.instance":"alpha"}}}]"#,
        );
        docker.inspect(
            "container",
            "ths-alpha-init",
            r#"[{"Id":"foreign-init","Config":{"Labels":{}}}]"#,
        );
        docker.inspect(
            "volume",
            "ths-alpha-wallet",
            r#"[{"Name":"ths-alpha-wallet","Labels":{"com.zakura.ths.instance":"alpha"}}]"#,
        );
        docker.inspect(
            "network",
            "ths-alpha",
            r#"[{"Id":"owned-network","Labels":{"com.zakura.ths.instance":"alpha"}}]"#,
        );
        let (mut host, events) = RecordingHost::new();
        host.shutdown_collision = Some(docker);
        let (_sender, shutdown) = Shutdown::channel();

        let error = runtime_for_tests()
            .start_with(&name("alpha"), true, false, 0, &host, &shutdown)
            .unwrap_err();

        assert!(format!("{error:#}").contains("ths-alpha-init is not owned"));
        assert!(
            events
                .lock()
                .unwrap()
                .contains(&"delete_partial:alpha".into())
        );
        assert_eq!(
            *host
                .shutdown_collision
                .as_ref()
                .unwrap()
                .runs
                .lock()
                .unwrap(),
            [
                "rm -f owned-app",
                "volume rm ths-alpha-wallet",
                "network rm owned-network",
            ]
        );
    }

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

    #[test]
    fn overlapping_faucet_process_rejects_the_locked_journal() {
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
    fn faucet_journal_lock_probe() {
        let Some(dir) = std::env::var_os("THS_FAUCET_JOURNAL_LOCK_PROBE") else {
            return;
        };
        let error = FaucetJournal::open(std::path::Path::new(&dir))
            .err()
            .expect("overlapping command acquired the lock");
        assert!(
            error
                .to_string()
                .contains("another faucet command is running")
        );
    }

    /// holds a child between fork and exec so it keeps every inherited descriptor.
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

    /// answers loopback requests in turn with canned statuses and bodies, passing on each
    /// request body.
    fn serve(responses: Vec<(&'static str, &'static str)>) -> (String, mpsc::Receiver<String>) {
        use std::io::{BufRead, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (requests, received) = mpsc::channel();
        std::thread::spawn(move || {
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                }
                let mut request = vec![0; length];
                reader.read_exact(&mut request).unwrap();
                requests.send(String::from_utf8(request).unwrap()).unwrap();
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        (url, received)
    }

    #[test]
    fn responses_decode_or_report_their_status_and_body() {
        let (url, _requests) = serve(vec![
            ("200 OK", r#"{"blocks":1,"hashes":["tip"]}"#),
            ("409 Conflict", "already claimed"),
            ("200 OK", "not json"),
        ]);
        let client = reqwest::blocking::Client::new();
        let get = || client.get(&url).send().unwrap();

        let mined: MineResult = decode_response(get()).unwrap();
        assert_eq!(mined.hashes, ["tip"]);
        let rejected = decode_response::<MineResult>(get())
            .unwrap_err()
            .to_string();
        assert_eq!(rejected, "rejected (409 Conflict): already claimed");
        let malformed = format!("{:#}", decode_response::<MineResult>(get()).unwrap_err());
        assert!(malformed.starts_with("decoding response: "), "{malformed}");
    }

    #[test]
    fn wallet_faucet_keeps_funding_after_a_failed_account_and_keeps_its_keys() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = Runtime {
            root: dir.path().to_path_buf(),
        };
        let alpha = name("alpha");
        fs::create_dir_all(runtime.instance_dir(&alpha)).unwrap();
        let pending = r#"{"id":"a","kind":"faucet","from_account":null,"to_account":2,"source_pool":"ironwood","destination_pool":"ironwood","amount_zatoshi":100,"txid":"t","block_hash":null,"status":"broadcast"}"#;
        let funded = r#"{"id":"b","kind":"faucet","from_account":null,"to_account":3,"source_pool":"ironwood","destination_pool":"ironwood","amount_zatoshi":100,"txid":"u","block_hash":"b","status":"confirmed"}"#;
        let (url, requests) = serve(vec![
            ("422 Unprocessable Entity", "no funds"),
            ("200 OK", pending),
            ("200 OK", funded),
        ]);

        let error = runtime
            .wallet_faucet_with(
                &alpha,
                &url,
                &reqwest::blocking::Client::new(),
                &[1, 2, 3],
                100,
                "ironwood",
                true,
            )
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "2 of 3 faucet requests are pending or failed"
        );

        let requests: Vec<serde_json::Value> = requests
            .try_iter()
            .map(|request| serde_json::from_str(&request).unwrap())
            .collect();
        assert_eq!(requests.len(), 3);
        let journal = FaucetJournal::open(&runtime.instance_dir(&alpha)).unwrap();
        for (account, request) in (1..=3).zip(&requests) {
            assert_eq!(request["account_id"], account);
            // the unfinished batch keeps every key, so a retry submits the same ones.
            assert_eq!(
                request["idempotency_key"],
                journal
                    .key_for(&format!("wallet:ironwood:100:1,2,3:{account}"))
                    .unwrap()
            );
        }
    }

    #[test]
    fn readiness_failure_deletes_the_started_instance() {
        let (mut host, events) = RecordingHost::new();
        host.wait_ready_result = Err("dashboard did not become healthy".into());
        let (_sender, shutdown) = Shutdown::channel();
        let err = runtime_for_tests()
            .start_with(&name("alpha"), false, false, 0, &host, &shutdown)
            .unwrap_err();
        assert!(err.to_string().contains("dashboard did not become healthy"));
        let events = events.lock().unwrap().clone();
        let allocate_pos = events
            .iter()
            .position(|e| e == "allocate:alpha")
            .expect("allocate:alpha");
        assert!(
            events[allocate_pos + 1..]
                .iter()
                .any(|e| e == "delete:alpha"),
            "expected delete:alpha after allocate:alpha, got {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|e| e.starts_with("delete:") && !e.ends_with("alpha"))
        );
        assert!(!events.iter().any(|e| e.starts_with("wait_for_shutdown")));
    }

    #[test]
    fn browser_open_failure_deletes_and_does_not_wait() {
        let (mut host, events) = RecordingHost::new();
        host.open_url_result = Err("opening http://127.0.0.1:1".into());
        let (_sender, shutdown) = Shutdown::channel();
        let err = runtime_for_tests()
            .start_with(&name("alpha"), false, false, 0, &host, &shutdown)
            .unwrap_err();
        assert!(err.to_string().contains("opening"));
        let events = events.lock().unwrap().clone();
        assert!(events.iter().any(|e| e.starts_with("open_url:")));
        assert!(events.iter().any(|e| e == "delete:alpha"));
        assert!(!events.iter().any(|e| e.starts_with("wait_for_shutdown")));
    }

    #[test]
    fn no_open_skips_browser_and_waits_for_shutdown() {
        let (host, events) = RecordingHost::new();
        let (sender, shutdown) = Shutdown::channel();
        let watched = std::sync::Arc::clone(&events);
        std::thread::spawn(move || {
            let started = std::time::Instant::now();
            while started.elapsed() < Duration::from_secs(2) {
                let waiting = watched
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|event| event.starts_with("wait_for_shutdown"));
                if waiting {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            sender.send(Ended::Interrupted).unwrap();
        });
        runtime_for_tests()
            .start_with(&name("alpha"), true, false, 0, &host, &shutdown)
            .unwrap();
        let events = events.lock().unwrap().clone();
        assert!(!events.iter().any(|e| e.starts_with("open_url:")));
        assert!(events.iter().any(|e| e.starts_with("wait_for_shutdown")));
        assert!(events.iter().any(|e| e == "delete:alpha"));
    }

    #[test]
    fn deletion_elsewhere_ends_start_without_deleting_again() {
        let (mut host, events) = RecordingHost::new();
        host.deleted_elsewhere = true;
        let (_sender, shutdown) = Shutdown::channel();
        runtime_for_tests()
            .start_with(&name("alpha"), true, false, 0, &host, &shutdown)
            .unwrap();
        let events = events.lock().unwrap().clone();
        let waited = events
            .iter()
            .position(|e| e == "wait_for_shutdown:app-1")
            .expect("waited on the app container");
        assert!(
            !events[waited..].iter().any(|e| e.starts_with("delete:")),
            "deleted again after another command deleted it: {events:?}"
        );
    }

    #[test]
    fn startup_and_cleanup_failures_are_both_reported_once() {
        let (mut host, events) = RecordingHost::new();
        host.wait_ready_result = Err("dashboard readiness timed out".into());
        host.final_cleanup_failure = Some("Docker inspection failed".into());
        let (_sender, shutdown) = Shutdown::channel();
        let error = runtime_for_tests()
            .start_with(&name("alpha"), true, false, 0, &host, &shutdown)
            .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("dashboard readiness timed out"));
        assert!(text.contains("Docker inspection failed"));
        assert_eq!(
            events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| *event == "final_cleanup:alpha")
                .count(),
            1
        );
    }

    #[test]
    fn interrupt_before_ready_deletes_the_started_instance() {
        let (mut host, events) = RecordingHost::new();
        host.interrupt_before_ready = true;
        let (_sender, shutdown) = Shutdown::channel();
        let err = runtime_for_tests()
            .start_with(&name("alpha"), false, false, 0, &host, &shutdown)
            .unwrap_err();
        assert!(err.to_string().contains("interrupted"));
        let events = events.lock().unwrap().clone();
        assert!(events.iter().any(|e| e == "allocate:alpha"));
        assert!(events.iter().any(|e| e == "delete:alpha"));
        assert!(!events.iter().any(|e| e.starts_with("wait_for_shutdown")));
    }

    #[test]
    fn interrupt_during_allocate_deletes_only_the_named_instance() {
        let (host, events) = RecordingHost::new();
        let (sender, shutdown) = Shutdown::channel();
        sender.send(Ended::Interrupted).unwrap();
        let err = runtime_for_tests()
            .start_with(&name("alpha"), false, false, 0, &host, &shutdown)
            .unwrap_err();
        assert!(err.to_string().contains("interrupted"));
        let events = events.lock().unwrap().clone();
        assert!(events.iter().any(|e| e == "delete:alpha"));
        assert!(!events.iter().any(|e| e.contains("beta")));
    }

    #[test]
    fn validates_instance_names() {
        for valid in ["default", "project-2", "a"] {
            assert!(valid.parse::<InstanceName>().is_ok());
        }
        for invalid in ["", "UPPER", "with space", "-start", "end-"] {
            assert!(invalid.parse::<InstanceName>().is_err());
        }
    }

    #[test]
    fn project_images_are_version_locked() {
        assert_eq!(
            app_image(),
            format!(
                "ghcr.io/zcashlabs/thus-spoke-zakura-app:{}",
                env!("CARGO_PKG_VERSION")
            )
        );
        assert_eq!(
            lightwalletd_image(),
            format!(
                "ghcr.io/zcashlabs/thus-spoke-zakura-lightwalletd:{}",
                env!("CARGO_PKG_VERSION")
            )
        );
    }

    #[test]
    fn shutdown_reports_interrupt_from_channel() {
        let (sender, shutdown) = Shutdown::channel();
        assert!(!shutdown.try_interrupted());
        sender.send(Ended::Interrupted).unwrap();
        assert!(shutdown.try_interrupted());
        assert!(shutdown.try_interrupted());
        assert_eq!(shutdown.wait().unwrap(), Ended::Interrupted);
    }

    #[test]
    fn shutdown_check_bails_when_latched() {
        let (sender, shutdown) = Shutdown::channel();
        shutdown.check().unwrap();
        sender.send(Ended::Interrupted).unwrap();
        let err = shutdown.check().unwrap_err();
        assert!(err.to_string().contains("interrupted"));
        let err = shutdown.check().unwrap_err();
        assert!(err.to_string().contains("interrupted"));
    }

    #[test]
    fn shutdown_wait_timeout_wakes_on_signal() {
        let (sender, shutdown) = Shutdown::channel();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            sender.send(Ended::Interrupted).unwrap();
        });
        let started = Instant::now();
        let err = shutdown.wait_timeout(Duration::from_secs(2)).unwrap_err();
        assert!(err.to_string().contains("interrupted"));
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn shutdown_wait_timeout_returns_on_idle() {
        let (_sender, shutdown) = Shutdown::channel();
        shutdown.wait_timeout(Duration::from_millis(20)).unwrap();
    }

    #[test]
    fn shutdown_wait_returns_after_signal() {
        let (sender, shutdown) = Shutdown::channel();
        sender.send(Ended::Interrupted).unwrap();
        assert_eq!(shutdown.wait().unwrap(), Ended::Interrupted);
    }

    #[test]
    fn shutdown_wait_returns_when_the_environment_is_gone() {
        let (sender, shutdown) = Shutdown::channel();
        sender.send(Ended::DeletedElsewhere).unwrap();
        assert_eq!(shutdown.wait().unwrap(), Ended::DeletedElsewhere);
        assert!(!shutdown.try_interrupted());
    }

    #[test]
    fn shutdown_wait_keeps_a_deletion_notice_that_arrived_first() {
        let (sender, shutdown) = Shutdown::channel();
        sender.send(Ended::DeletedElsewhere).unwrap();
        sender.send(Ended::Interrupted).unwrap();
        assert_eq!(shutdown.wait().unwrap(), Ended::DeletedElsewhere);
        assert!(shutdown.try_interrupted());
    }

    fn watched(id: &str) -> WatchedContainer {
        WatchedContainer {
            id: id.to_owned(),
            created: "2026-01-01T00:00:00Z".to_owned(),
        }
    }

    /// Scripted Docker answers for `DeletionMonitor`. An unscripted `exists`
    /// fails like an unreachable daemon; an unscripted `wait_destroyed` blocks like a quiet
    /// event stream until cancelled.
    #[derive(Default)]
    struct ScriptedEvents {
        destroyed: Option<WatchedContainer>,
        streams: Mutex<VecDeque<Result<bool, String>>>,
        exists: Mutex<VecDeque<Result<bool, String>>>,
        calls: Mutex<Vec<String>>,
        cancelled: AtomicBool,
    }

    impl ScriptedEvents {
        fn record(&self, call: String) {
            self.calls.lock().unwrap().push(call);
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn wait_for_calls(&self, count: usize) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.calls.lock().unwrap().len() < count {
                assert!(
                    Instant::now() < deadline,
                    "calls so far: {:?}",
                    self.calls()
                );
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }

    fn scripted<T>(queue: &Mutex<VecDeque<Result<T, String>>>) -> Option<Result<T>> {
        queue
            .lock()
            .unwrap()
            .pop_front()
            .map(|answer| answer.map_err(|e| anyhow!("{e}")))
    }

    impl ContainerEvents for ScriptedEvents {
        fn exists(&self, id: &str) -> Result<bool> {
            self.record(format!("exists:{id}"));
            scripted(&self.exists).unwrap_or_else(|| Err(anyhow!("Docker is unreachable")))
        }

        fn wait_destroyed(&self, container: &WatchedContainer) -> Result<bool> {
            self.record(format!("wait_destroyed:{}", container.id));
            if let Some(answer) = scripted(&self.streams) {
                return answer;
            }
            if self.destroyed.as_ref() == Some(container) {
                return Ok(true);
            }
            while !self.cancelled.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(1));
            }
            Ok(false)
        }

        fn cancel(&self) {
            self.cancelled.store(true, Ordering::SeqCst);
        }
    }

    fn monitor(events: &Arc<ScriptedEvents>, shutdown: &Shutdown) -> DeletionMonitor {
        DeletionMonitor::spawn(
            events.clone(),
            &watched("app-1"),
            Duration::from_millis(1),
            shutdown,
        )
    }

    fn replacement_before_monitoring(delayed_by_error: bool) {
        // The original is gone and a replacement has already taken its name. Event replay
        // must use the allocated ID and creation time, even if Docker initially fails.
        let events = Arc::new(ScriptedEvents {
            destroyed: Some(watched("app-1")),
            ..ScriptedEvents::default()
        });
        if delayed_by_error {
            events
                .streams
                .lock()
                .unwrap()
                .push_back(Err("Docker is unreachable".into()));
        }
        let (_sender, shutdown) = Shutdown::channel();
        let monitor = monitor(&events, &shutdown);
        let ended = shutdown.receiver.recv_timeout(Duration::from_secs(5));
        drop(monitor);
        assert_eq!(ended.unwrap(), Ended::DeletedElsewhere);
        assert!(events.calls().iter().all(|call| call.ends_with(":app-1")));
    }

    #[test]
    fn deletion_monitor_uses_allocation_identity_when_replaced_before_monitoring() {
        replacement_before_monitoring(false);
    }

    #[test]
    fn deletion_monitor_uses_allocation_identity_after_initial_docker_errors() {
        replacement_before_monitoring(true);
    }

    #[test]
    fn deletion_monitor_reports_a_destroy_event() {
        let events = Arc::new(ScriptedEvents::default());
        events.streams.lock().unwrap().push_back(Ok(true));
        let (_sender, shutdown) = Shutdown::channel();
        let _monitor = monitor(&events, &shutdown);
        assert_eq!(shutdown.wait().unwrap(), Ended::DeletedElsewhere);
        assert_eq!(events.calls(), ["wait_destroyed:app-1"]);
    }

    #[test]
    fn deletion_monitor_reports_a_container_already_gone() {
        let events = Arc::new(ScriptedEvents::default());
        events.streams.lock().unwrap().push_back(Ok(false));
        events.exists.lock().unwrap().push_back(Ok(false));
        let (_sender, shutdown) = Shutdown::channel();
        let _monitor = monitor(&events, &shutdown);
        assert_eq!(shutdown.wait().unwrap(), Ended::DeletedElsewhere);
    }

    #[test]
    fn deletion_monitor_treats_docker_errors_as_unknown() {
        let events = Arc::new(ScriptedEvents::default());
        events
            .streams
            .lock()
            .unwrap()
            .extend([Err("Docker is unreachable".to_owned()), Ok(false)]);
        let (sender, shutdown) = Shutdown::channel();
        let monitor = monitor(&events, &shutdown);
        // Both reconciliations fail like an unreachable daemon; the third stream stays open.
        events.wait_for_calls(5);
        sender.send(Ended::Interrupted).unwrap();
        assert_eq!(shutdown.wait().unwrap(), Ended::Interrupted);
        drop(monitor);
        assert_eq!(
            events.calls(),
            [
                "wait_destroyed:app-1",
                "exists:app-1",
                "wait_destroyed:app-1",
                "exists:app-1",
                "wait_destroyed:app-1",
            ]
        );
        assert!(shutdown.receiver.try_recv().is_err(), "reported a deletion");
    }

    #[test]
    fn deletion_monitor_resubscribes_after_a_disconnect() {
        let events = Arc::new(ScriptedEvents::default());
        events.streams.lock().unwrap().extend([Ok(false), Ok(true)]);
        events.exists.lock().unwrap().push_back(Ok(true));
        let (_sender, shutdown) = Shutdown::channel();
        let _monitor = monitor(&events, &shutdown);
        assert_eq!(shutdown.wait().unwrap(), Ended::DeletedElsewhere);
        assert_eq!(
            events.calls(),
            [
                "wait_destroyed:app-1",
                "exists:app-1",
                "wait_destroyed:app-1",
            ]
        );
    }

    #[test]
    fn deletion_monitor_follows_the_original_container_not_its_name() {
        // A disconnect hides the destroy event, and a new container already took the name.
        let events = Arc::new(ScriptedEvents::default());
        events.streams.lock().unwrap().push_back(Ok(false));
        events.exists.lock().unwrap().push_back(Ok(false));
        let (_sender, shutdown) = Shutdown::channel();
        let _monitor = monitor(&events, &shutdown);
        assert_eq!(shutdown.wait().unwrap(), Ended::DeletedElsewhere);
        assert_eq!(events.calls(), ["wait_destroyed:app-1", "exists:app-1",]);
    }

    #[test]
    fn dropping_the_deletion_monitor_stops_the_watch() {
        let events = Arc::new(ScriptedEvents::default());
        let (_sender, shutdown) = Shutdown::channel();
        let monitor = monitor(&events, &shutdown);
        events.wait_for_calls(1);
        drop(monitor);
        assert!(events.cancelled.load(Ordering::SeqCst));
        assert!(shutdown.receiver.try_recv().is_err(), "reported a deletion");
    }

    struct IdleDocker;

    impl DockerResourceCommands for IdleDocker {
        fn output(&self, args: &[&str]) -> Result<String> {
            bail!("unexpected Docker read: {args:?}")
        }

        fn run(&self, args: &[&str]) -> Result<()> {
            bail!("unexpected Docker run: {args:?}")
        }
    }

    #[test]
    fn wait_ready_aborts_when_shutdown_is_signaled() {
        let (sender, shutdown) = Shutdown::channel();
        sender.send(Ended::Interrupted).unwrap();
        let err = readiness::wait_ready(
            "http://127.0.0.1:1",
            "missing-app",
            Deadline::after(Duration::from_secs(5)),
            &shutdown,
            &LifecyclePolicy::default(),
            &IdleDocker,
        )
        .unwrap_err();
        assert!(err.to_string().contains("interrupted"));
    }

    #[test]
    fn wait_for_zakura_tip_aborts_when_shutdown_is_signaled() {
        let (sender, shutdown) = Shutdown::channel();
        sender.send(Ended::Interrupted).unwrap();
        let err = readiness::wait_for_zakura_tip(
            "http://127.0.0.1:1",
            "missing-zakura",
            Deadline::after(Duration::from_secs(5)),
            &shutdown,
            &LifecyclePolicy::default(),
            &IdleDocker,
        )
        .unwrap_err();
        assert!(err.to_string().contains("interrupted"));
    }

    #[test]
    fn default_offset_uses_stable_loopback_ports() {
        let ports = host_ports(0).unwrap();
        assert_eq!(ports.dashboard, 32805);
        assert_eq!(ports.rpc, 18232);
        assert_eq!(ports.p2p, 18233);
        assert_eq!(ports.lightwalletd, 9067);
        assert_eq!(loopback_publish(ports.rpc, 18232), "127.0.0.1:18232:18232");
        assert_eq!(
            loopback_publish(ports.dashboard, 8080),
            "127.0.0.1:32805:8080"
        );
    }

    #[test]
    fn port_offset_shifts_all_four_hosts_by_the_same_stride() {
        let base = host_ports(0).unwrap();
        let shifted = host_ports(10).unwrap();
        assert_eq!(shifted.dashboard, base.dashboard + 10);
        assert_eq!(shifted.rpc, base.rpc + 10);
        assert_eq!(shifted.p2p, base.p2p + 10);
        assert_eq!(shifted.lightwalletd, base.lightwalletd + 10);
    }

    #[test]
    fn port_offset_rejects_values_that_are_not_multiples_of_ten() {
        let err = host_ports(1).unwrap_err();
        assert!(err.to_string().contains('1'));
    }

    #[test]
    fn port_offset_accepts_the_largest_value_that_keeps_every_port_in_range() {
        let ports = host_ports(32730).unwrap();
        assert_eq!(ports.dashboard, 65535);
    }

    #[test]
    fn port_offset_rejects_values_that_overflow_a_host_port() {
        let err = host_ports(32740).unwrap_err();
        assert!(err.to_string().contains("overflow"));
    }

    #[test]
    fn invalid_port_offset_is_rejected_before_any_deletion_or_allocation() {
        let (mut host, events) = RecordingHost::new();
        host.initial_delete_error = true;
        let (_sender, shutdown) = Shutdown::channel();

        let error = runtime_for_tests()
            .start_with(&name("alpha"), false, false, 32740, &host, &shutdown)
            .unwrap_err();

        assert!(error.to_string().contains("overflow"));
        assert!(events.lock().unwrap().is_empty());
    }

    #[test]
    fn reports_the_conflicting_loopback_port() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let err = require_free_loopback(port).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains(&port.to_string()),
            "error should name port {port}, got {message}"
        );
    }

    #[test]
    fn endpoints_json_includes_regtest_and_plaintext_lightwalletd() {
        let json = serde_json::to_value(endpoints_for(&host_ports(0).unwrap())).unwrap();
        assert_eq!(json["dashboard"], "http://127.0.0.1:32805");
        assert_eq!(json["rpc"], "http://127.0.0.1:18232");
        assert_eq!(json["lightwalletd"], "http://127.0.0.1:9067");
        assert_eq!(json["p2p"], "127.0.0.1:18233");
        assert_eq!(json["network"], "regtest");
        assert_eq!(json["tls"], false);
    }

    #[test]
    fn endpoints_json_without_network_fields_still_deserializes() {
        let parsed: Endpoints = serde_json::from_str(
            r#"{"dashboard":"http://127.0.0.1:1","rpc":"http://127.0.0.1:2","lightwalletd":"http://127.0.0.1:3","p2p":"127.0.0.1:4"}"#,
        )
        .unwrap();
        assert_eq!(parsed.network, "regtest");
        assert!(!parsed.tls);
    }
}
