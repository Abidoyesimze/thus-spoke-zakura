use super::*;

#[derive(Clone, Debug)]
pub struct InstanceName(pub(super) String);

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

pub(super) fn default_regtest() -> String {
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
pub(crate) struct Instance {
    pub(super) name: String,
    pub(super) version: u32,
    pub(crate) endpoints: Endpoints,
    /// unix seconds; missing for environments created before it was recorded.
    #[serde(default)]
    pub(super) created_at: Option<u64>,
}

impl Runtime {
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

    pub(crate) fn instance_dir(&self, name: &InstanceName) -> PathBuf {
        self.root.join(name.to_string())
    }

    pub(super) fn write_instance(&self, name: &InstanceName, endpoints: &Endpoints) -> Result<()> {
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

    pub(crate) fn read_instance(&self, name: &InstanceName) -> Result<Instance> {
        let path = self.instance_dir(name).join("instance.json");
        serde_json::from_slice(
            &fs::read(&path).with_context(|| format!("instance {name} does not exist"))?,
        )
        .context("invalid instance metadata")
    }

    pub(super) fn recovery_path(&self, name: &InstanceName) -> PathBuf {
        self.instance_dir(name).join(recovery::RECOVERY_FILE)
    }
}

#[derive(Debug, Default)]
pub(super) struct HostPorts {
    pub(super) dashboard: u16,
    pub(super) rpc: u16,
    pub(super) p2p: u16,
    pub(super) lightwalletd: u16,
}

pub(super) fn host_ports(offset: u16) -> Result<HostPorts> {
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

pub(super) fn loopback_publish(host: u16, container: u16) -> String {
    if host == 0 {
        format!("127.0.0.1::{container}")
    } else {
        format!("127.0.0.1:{host}:{container}")
    }
}

pub(super) fn require_free_loopback(port: u16) -> Result<()> {
    match std::net::TcpListener::bind(("127.0.0.1", port)) {
        Ok(listener) => {
            drop(listener);
            Ok(())
        }
        Err(_) => bail!("port {port} is already in use on 127.0.0.1"),
    }
}

pub(crate) fn prefix(name: &InstanceName) -> String {
    format!("ths-{name}")
}
pub(super) fn label(name: &InstanceName) -> String {
    format!("{INSTANCE_LABEL}={name}")
}

pub(super) fn endpoints_for(ports: &HostPorts) -> Endpoints {
    Endpoints {
        dashboard: format!("http://127.0.0.1:{}", ports.dashboard),
        rpc: format!("http://127.0.0.1:{}", ports.rpc),
        lightwalletd: format!("http://127.0.0.1:{}", ports.lightwalletd),
        p2p: format!("127.0.0.1:{}", ports.p2p),
        network: default_regtest(),
        tls: false,
    }
}

pub(super) fn status_text(name: &InstanceName, running: bool, e: &Endpoints) -> String {
    let state = if running { "running" } else { "stopped" };
    format!("{name}: {state}\n{}", endpoint_lines(e))
}
pub(super) fn endpoint_lines(e: &Endpoints) -> String {
    format!(
        "  Dashboard    {}\n  Zakura RPC   {}\n  lightwalletd {}  (network={}, tls={})\n  P2P          {}",
        e.dashboard, e.rpc, e.lightwalletd, e.network, e.tls, e.p2p
    )
}

pub(super) struct InstanceLock {
    file: File,
    path: PathBuf,
    remove_on_drop: bool,
}

fn lock_registry(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)?;
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(TryLockError::WouldBlock) => bail!("instance lock registry is busy; try again"),
            Err(TryLockError::Error(error)) => {
                return Err(error).context("locking instance registry");
            }
        }
    }
}

impl InstanceLock {
    pub(super) fn remove_when_dropped(&mut self) {
        self.remove_on_drop = true;
    }

    pub(super) fn finish(mut self) -> Result<()> {
        self.remove_on_drop = false;
        let _registry = lock_registry(&self.path.with_file_name(".registry.lock"))?;
        match fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("removing the instance lock"),
        }
        self.file.unlock().context("releasing the instance lock")
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        let registry = if self.remove_on_drop {
            match lock_registry(&self.path.with_file_name(".registry.lock")) {
                Ok(registry) => Some(registry),
                Err(error) => {
                    eprintln!("could not remove the instance lock: {error:#}");
                    None
                }
            }
        } else {
            None
        };
        if registry.is_some()
            && let Err(error) = fs::remove_file(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("could not remove the instance lock: {error}");
        }
        if let Err(error) = self.file.unlock() {
            eprintln!("could not release the instance lock: {error}");
        }
    }
}

impl Runtime {
    pub(super) fn lock_instance(&self, name: &InstanceName) -> Result<InstanceLock> {
        let lock_dir = self.root.join(".locks");
        fs::create_dir_all(&lock_dir)?;
        // Serialize opening and unlinking locks so a contender cannot lock an
        // unlinked inode while another command locks a newly created file.
        let _registry = lock_registry(&lock_dir.join(".registry.lock"))?;
        let path = lock_dir.join(format!("{name}.lock"));
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)?;
        match file.try_lock() {
            Ok(()) => Ok(InstanceLock {
                file,
                path,
                remove_on_drop: false,
            }),
            Err(TryLockError::WouldBlock) => {
                bail!("environment {name} is already in use by another command")
            }
            Err(TryLockError::Error(error)) => Err(error).context("locking environment"),
        }
    }
}

impl Runtime {
    pub(super) fn environments(
        &self,
        docker: &impl DockerResourceCommands,
    ) -> Result<Vec<EnvironmentStatus>> {
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
}

/// one container's docker state (`running`, `paused`, `exited`, ...), `missing`, or `foreign`
/// when a container this instance doesn't own holds its name, with its unix-second creation and
/// start times.
#[derive(Debug, Serialize)]
pub(super) struct ContainerStatus {
    pub(super) service: &'static str,
    pub(super) state: String,
    pub(super) created_at: Option<u64>,
    pub(super) started_at: Option<u64>,
}

#[derive(Debug, Serialize)]
pub(super) struct EnvironmentStatus {
    #[serde(flatten)]
    pub(super) instance: Instance,
    /// `running`, `paused` (every service up, some paused), `degraded` (some services up),
    /// `stopped`, `conflict` when another owner holds one of its container names, or `unknown`
    /// when docker can't be read.
    pub(super) state: &'static str,
    pub(super) containers: Vec<ContainerStatus>,
}

pub(super) const SERVICES: [&str; 4] = ["app", "zakura", "lightwalletd", "init"];

/// keeps an environment whose containers can't be read, such as while docker is stopped, with
/// its live status unknown.
pub(super) fn environment_status(
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
pub(super) fn environment_state(containers: &[ContainerStatus]) -> &'static str {
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

pub(super) fn container_status(
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
pub(super) fn parse_docker_time(value: &str) -> Option<u64> {
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

pub(super) fn now_unix() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before 1970")?
        .as_secs())
}

pub(super) fn ago(now: u64, then: u64) -> String {
    let seconds = now.saturating_sub(then);
    match seconds {
        0..60 => format!("{seconds}s ago"),
        60..3_600 => format!("{}m ago", seconds / 60),
        3_600..86_400 => format!("{}h ago", seconds / 3_600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

pub(super) fn render_list(environments: &[EnvironmentStatus], now: u64) -> String {
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
