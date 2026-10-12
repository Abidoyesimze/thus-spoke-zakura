use super::*;

/// How long deletion monitoring waits before retrying after a Docker error or a lost event stream.
pub(super) const DELETION_WATCH_RETRY: Duration = Duration::from_secs(1);

/// The container whose deletion a running `start` watches. Its ID keeps a same-name replacement
/// from hiding the deletion, and its creation time (Docker's clock) lets a subscription replay
/// a destroy event that happened before it connected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct WatchedContainer {
    pub(super) id: String,
    pub(super) created: String,
}

/// The Docker operations behind deletion monitoring, and its test seam.
pub(super) trait ContainerEvents: Send + Sync {
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
pub(super) struct DockerEvents {
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
pub(super) fn watch_deletion(
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
pub(super) struct DeletionMonitor {
    events: Arc<dyn ContainerEvents>,
    stop: Option<mpsc::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl DeletionMonitor {
    pub(super) fn spawn(
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
