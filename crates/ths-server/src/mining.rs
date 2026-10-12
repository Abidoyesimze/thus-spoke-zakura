use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use serde::Serialize;
use tokio::sync::{Mutex, watch};

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MiningState {
    Mining,
    Syncing,
    Completed,
    Failed,
}

#[derive(Clone, Debug, Serialize)]
pub struct MiningJob {
    pub id: String,
    pub requested_blocks: u32,
    pub completed_blocks: u32,
    pub state: MiningState,
    pub error: Option<String>,
    pub progress_uncertain: bool,
}

#[async_trait::async_trait]
pub trait MiningRuntime: Send + Sync + 'static {
    async fn generate_one(&self) -> anyhow::Result<String>;
    async fn synchronize(&self, tip_hash: &str) -> anyhow::Result<()>;
    fn notify(&self, topic: &'static str);
}

#[derive(Clone)]
pub struct MiningUpdate {
    pub job: MiningJob,
    pub hashes: Option<Arc<Vec<String>>>,
}

pub struct MiningCoordinator {
    state: Mutex<CoordinatorState>,
    capacity: usize,
}

struct CoordinatorState {
    jobs: HashMap<String, MiningJob>,
    keys: HashMap<String, String>,
    latest_id: Option<String>,
    active_id: Option<String>,
    latest_updates: Option<watch::Sender<MiningUpdate>>,
}

pub struct Admission {
    pub job: MiningJob,
    pub updates: watch::Receiver<MiningUpdate>,
}

#[derive(Debug, thiserror::Error)]
pub enum MiningAdmissionError {
    #[error("blocks must be between 1 and 10000")]
    InvalidBlocks,
    #[error("idempotency_key must contain 8-128 visible ASCII characters without spaces")]
    InvalidKey,
    #[error("idempotency_key was already used with a different block count")]
    KeyConflict,
    #[error("A manual mining job is already active")]
    Busy,
    #[error(
        "Mining job capacity reached (1024 jobs per server lifetime); existing jobs remain available"
    )]
    Capacity,
}

pub(crate) fn validate_idempotency_key(key: &str) -> Result<(), &'static str> {
    if !(8..=128).contains(&key.len()) {
        Err("idempotency_key must contain 8-128 characters")
    } else if !key.bytes().all(|byte| byte.is_ascii_graphic()) {
        Err("idempotency_key must contain only visible ASCII characters")
    } else {
        Ok(())
    }
}

impl MiningCoordinator {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(CoordinatorState {
                jobs: HashMap::new(),
                keys: HashMap::new(),
                latest_id: None,
                active_id: None,
                latest_updates: None,
            }),
            capacity: 1024,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity,
            ..Self::new()
        }
    }

    pub async fn start(
        self: &Arc<Self>,
        runtime: Arc<dyn MiningRuntime>,
        blocks: u32,
        key: String,
    ) -> Result<Admission, MiningAdmissionError> {
        if !(1..=10_000).contains(&blocks) {
            return Err(MiningAdmissionError::InvalidBlocks);
        }
        if validate_idempotency_key(&key).is_err() {
            return Err(MiningAdmissionError::InvalidKey);
        }
        let mut state = self.state.lock().await;
        if let Some(id) = state.keys.get(&key) {
            let job = state
                .jobs
                .get(id)
                .expect("Every mining key has a snapshot")
                .clone();
            if job.requested_blocks != blocks {
                return Err(MiningAdmissionError::KeyConflict);
            }
            let updates = if state.latest_id.as_ref() == Some(id) {
                state
                    .latest_updates
                    .as_ref()
                    .expect("Latest mining job has a channel")
                    .subscribe()
            } else {
                watch::channel(MiningUpdate {
                    job: job.clone(),
                    hashes: None,
                })
                .1
            };
            return Ok(Admission { job, updates });
        }
        if state.active_id.is_some() {
            return Err(MiningAdmissionError::Busy);
        }
        if state.jobs.len() >= self.capacity {
            return Err(MiningAdmissionError::Capacity);
        }
        let job = MiningJob {
            id: uuid::Uuid::new_v4().to_string(),
            requested_blocks: blocks,
            completed_blocks: 0,
            state: MiningState::Mining,
            error: None,
            progress_uncertain: false,
        };
        let (sender, updates) = watch::channel(MiningUpdate {
            job: job.clone(),
            hashes: None,
        });
        state.keys.insert(key, job.id.clone());
        state.jobs.insert(job.id.clone(), job.clone());
        state.latest_id = Some(job.id.clone());
        state.active_id = Some(job.id.clone());
        state.latest_updates = Some(sender);

        // Both tasks are detached from the request; only the supervisor owns the worker handle.
        let worker_jobs = Arc::clone(self);
        let worker_runtime = Arc::clone(&runtime);
        let worker_job = job.clone();
        let worker = tokio::spawn(async move {
            worker_jobs.run(worker_runtime, worker_job).await;
        });
        let supervisor_jobs = Arc::clone(self);
        let id = job.id.clone();
        tokio::spawn(async move {
            if let Err(error) = worker.await {
                tracing::error!(job_id = %id, %error, "Mining worker failed");
                let mut job = supervisor_jobs
                    .get(&id)
                    .await
                    .expect("Worker job has a snapshot");
                if !matches!(job.state, MiningState::Completed | MiningState::Failed) {
                    job.state = MiningState::Failed;
                    job.progress_uncertain = true;
                    job.error = Some(format!("Mining worker failed: {error}"));
                    supervisor_jobs
                        .publish(MiningUpdate { job, hashes: None })
                        .await;
                    runtime.notify("mining");
                    runtime.notify("chain");
                }
            }
        });
        Ok(Admission { job, updates })
    }

    pub async fn latest(&self) -> Option<MiningJob> {
        let state = self.state.lock().await;
        state
            .latest_id
            .as_ref()
            .and_then(|id| state.jobs.get(id))
            .cloned()
    }

    pub async fn get(&self, id: &str) -> Option<MiningJob> {
        self.state.lock().await.jobs.get(id).cloned()
    }

    async fn publish(&self, update: MiningUpdate) {
        let sender = {
            let mut state = self.state.lock().await;
            state.jobs.insert(update.job.id.clone(), update.job.clone());
            if matches!(
                update.job.state,
                MiningState::Completed | MiningState::Failed
            ) && state.active_id.as_ref() == Some(&update.job.id)
            {
                state.active_id = None;
            }
            if state.latest_id.as_ref() == Some(&update.job.id) {
                state.latest_updates.clone()
            } else {
                None
            }
        };
        if let Some(sender) = sender {
            // Publication must succeed even after every HTTP waiter disconnects.
            sender.send_replace(update);
        }
    }

    async fn run(&self, runtime: Arc<dyn MiningRuntime>, mut job: MiningJob) {
        let mut hashes = Vec::with_capacity(job.requested_blocks as usize);
        let generation: anyhow::Result<()> = async {
            for _ in 0..job.requested_blocks {
                hashes.push(runtime.generate_one().await?);
                job.completed_blocks = hashes.len() as u32;
                self.publish(MiningUpdate {
                    job: job.clone(),
                    hashes: None,
                })
                .await;
                runtime.notify("mining");
            }
            Ok(())
        }
        .await;
        if let Err(error) = generation {
            job.state = MiningState::Failed;
            job.progress_uncertain = true;
            job.error = Some(format!("Mining failed: {error:#}"));
            self.publish(MiningUpdate { job, hashes: None }).await;
            runtime.notify("mining");
            runtime.notify("chain");
            return;
        }
        job.state = MiningState::Syncing;
        self.publish(MiningUpdate {
            job: job.clone(),
            hashes: None,
        })
        .await;
        runtime.notify("mining");
        runtime.notify("chain");
        let synchronization: anyhow::Result<()> = async {
            let tip = hashes.last().context("Mining returned no hashes")?;
            runtime.synchronize(tip).await
        }
        .await;
        let hashes = match synchronization {
            Ok(()) => {
                job.state = MiningState::Completed;
                Some(Arc::new(hashes))
            }
            Err(error) => {
                job.state = MiningState::Failed;
                job.error = Some(format!(
                    "Blocks mined, but wallet synchronization failed: {error:#}"
                ));
                None
            }
        };
        self.publish(MiningUpdate { job, hashes }).await;
        runtime.notify("mining");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;
    use tokio::sync::Semaphore;

    struct FakeRuntime {
        permits: Semaphore,
        sync_permits: Semaphore,
        calls: AtomicU32,
        scans: AtomicU32,
        fail_call: u32,
        panic_call: u32,
        fail_sync: bool,
    }

    impl FakeRuntime {
        fn blocked() -> Self {
            Self {
                permits: Semaphore::new(0),
                sync_permits: Semaphore::new(1),
                calls: AtomicU32::new(0),
                scans: AtomicU32::new(0),
                fail_call: 0,
                panic_call: 0,
                fail_sync: false,
            }
        }

        fn release(&self, count: u32) {
            self.permits.add_permits(count as usize);
        }

        fn calls(&self) -> u32 {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl MiningRuntime for FakeRuntime {
        async fn generate_one(&self) -> anyhow::Result<String> {
            self.permits.acquire().await.unwrap().forget();
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            assert_ne!(call, self.panic_call, "injected worker panic");
            anyhow::ensure!(call != self.fail_call, "injected RPC failure");
            Ok(format!("{call:064x}"))
        }

        async fn synchronize(&self, tip_hash: &str) -> anyhow::Result<()> {
            assert_eq!(tip_hash.len(), 64);
            self.scans.fetch_add(1, Ordering::SeqCst);
            self.sync_permits.acquire().await.unwrap().forget();
            anyhow::ensure!(!self.fail_sync, "injected sync failure");
            Ok(())
        }

        fn notify(&self, _topic: &'static str) {}
    }

    async fn terminal(mut updates: tokio::sync::watch::Receiver<MiningUpdate>) -> MiningUpdate {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let update = updates.borrow_and_update().clone();
                if matches!(
                    update.job.state,
                    MiningState::Completed | MiningState::Failed
                ) {
                    return update;
                }
                updates.changed().await.unwrap();
            }
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn mining_continues_without_request_receiver() {
        let runtime = Arc::new(FakeRuntime::blocked());
        let jobs = Arc::new(MiningCoordinator::new());
        let admission = jobs
            .start(runtime.clone(), 3, "disconnect-test".into())
            .await
            .unwrap();
        let id = admission.job.id;
        let observer = admission.updates.clone();
        drop(admission.updates);
        runtime.release(3);
        let update = terminal(observer).await;
        assert_eq!(update.job.state, MiningState::Completed);
        assert_eq!(update.hashes.unwrap().len(), 3);
        assert_eq!(jobs.get(&id).await.unwrap().completed_blocks, 3);
        assert_eq!(runtime.calls(), 3);
        assert_eq!(runtime.scans.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn mining_continues_with_zero_receivers() {
        let runtime = Arc::new(FakeRuntime::blocked());
        let jobs = Arc::new(MiningCoordinator::new());
        let admission = jobs
            .start(runtime.clone(), 2, "zero-receivers".into())
            .await
            .unwrap();
        drop(admission.updates);
        runtime.release(2);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let replay = jobs
                    .start(runtime.clone(), 2, "zero-receivers".into())
                    .await
                    .unwrap();
                if replay.job.state == MiningState::Completed {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(runtime.calls(), 2);
    }

    #[tokio::test]
    async fn concurrent_replay_has_one_worker_and_conflicts_are_rejected() {
        let runtime = Arc::new(FakeRuntime::blocked());
        let jobs = Arc::new(MiningCoordinator::new());
        let (first, replay) = tokio::join!(
            jobs.start(runtime.clone(), 2, "same-key".into()),
            jobs.start(runtime.clone(), 2, "same-key".into()),
        );
        let first = first.unwrap();
        assert_eq!(first.job.id, replay.unwrap().job.id);
        assert!(matches!(
            jobs.start(runtime.clone(), 3, "same-key".into()).await,
            Err(MiningAdmissionError::KeyConflict)
        ));
        assert!(matches!(
            jobs.start(runtime.clone(), 2, "other-key".into()).await,
            Err(MiningAdmissionError::Busy)
        ));
        runtime.release(2);
        terminal(first.updates).await;
        assert_eq!(runtime.calls(), 2);
    }

    #[tokio::test]
    async fn validates_blocks_and_keys() {
        let runtime = Arc::new(FakeRuntime::blocked());
        let jobs = Arc::new(MiningCoordinator::new());
        for blocks in [0, 10_001] {
            assert!(matches!(
                jobs.start(runtime.clone(), blocks, "valid-key".into())
                    .await,
                Err(MiningAdmissionError::InvalidBlocks)
            ));
        }
        for key in [
            "short".into(),
            "x".repeat(129),
            "space key".into(),
            "nonasciié".into(),
            "control\n".into(),
        ] {
            assert!(matches!(
                jobs.start(runtime.clone(), 1, key).await,
                Err(MiningAdmissionError::InvalidKey)
            ));
        }
        assert!(jobs.latest().await.is_none());
    }

    #[tokio::test]
    async fn capacity_keeps_old_replays_and_history_without_eviction() {
        let runtime = Arc::new(FakeRuntime::blocked());
        let jobs = Arc::new(MiningCoordinator::with_capacity(2));
        let first = jobs
            .start(runtime.clone(), 1, "first-key".into())
            .await
            .unwrap();
        let first_id = first.job.id;
        runtime.release(1);
        terminal(first.updates).await;
        runtime.sync_permits.add_permits(1);
        let second = jobs
            .start(runtime.clone(), 1, "second-key".into())
            .await
            .unwrap();
        let second_id = second.job.id;
        runtime.release(1);
        terminal(second.updates).await;
        assert!(matches!(
            jobs.start(runtime.clone(), 1, "third-key".into()).await,
            Err(MiningAdmissionError::Capacity)
        ));
        let replay = jobs
            .start(runtime.clone(), 1, "first-key".into())
            .await
            .unwrap();
        assert_eq!(replay.job.id, first_id);
        assert_eq!(replay.updates.borrow().job.id, first_id);
        assert!(replay.updates.borrow().hashes.is_none());
        assert_eq!(jobs.latest().await.unwrap().id, second_id);
        assert!(jobs.get(&first_id).await.is_some());
        assert!(jobs.get("unknown").await.is_none());
        assert_eq!(runtime.calls(), 2);
    }

    #[tokio::test]
    async fn rpc_failure_preserves_acknowledged_count_without_retry() {
        let runtime = Arc::new(FakeRuntime {
            fail_call: 2,
            ..FakeRuntime::blocked()
        });
        let jobs = Arc::new(MiningCoordinator::new());
        let first = jobs
            .start(runtime.clone(), 3, "failure-key".into())
            .await
            .unwrap();
        let id = first.job.id;
        runtime.release(3);
        let failed = terminal(first.updates).await.job;
        assert_eq!(failed.state, MiningState::Failed);
        assert_eq!(failed.completed_blocks, 1);
        assert!(failed.progress_uncertain);
        assert!(failed.error.unwrap().contains("injected RPC failure"));
        let replay = jobs
            .start(runtime.clone(), 3, "failure-key".into())
            .await
            .unwrap();
        assert_eq!(replay.job.id, id);
        assert_eq!(replay.job.state, MiningState::Failed);
        assert_eq!(runtime.calls(), 2);
        assert_eq!(runtime.scans.load(Ordering::SeqCst), 0);
        runtime.sync_permits.add_permits(1);
        let next = jobs
            .start(runtime.clone(), 1, "next-key".into())
            .await
            .unwrap();
        terminal(next.updates).await;
        assert_eq!(runtime.calls(), 3);
    }

    #[tokio::test]
    async fn synchronization_failure_has_full_known_count() {
        let runtime = Arc::new(FakeRuntime {
            fail_sync: true,
            ..FakeRuntime::blocked()
        });
        let jobs = Arc::new(MiningCoordinator::new());
        let admission = jobs
            .start(runtime.clone(), 2, "sync-failure".into())
            .await
            .unwrap();
        runtime.release(2);
        let failed = terminal(admission.updates).await.job;
        assert_eq!(failed.state, MiningState::Failed);
        assert_eq!(failed.completed_blocks, 2);
        assert!(!failed.progress_uncertain);
        assert!(
            failed
                .error
                .unwrap()
                .starts_with("Blocks mined, but wallet synchronization failed")
        );
        assert_eq!(runtime.calls(), 2);
    }

    #[tokio::test]
    async fn syncing_keeps_admission_busy() {
        let runtime = Arc::new(FakeRuntime {
            sync_permits: Semaphore::new(0),
            ..FakeRuntime::blocked()
        });
        let jobs = Arc::new(MiningCoordinator::new());
        let mut admission = jobs
            .start(runtime.clone(), 1, "syncing-key".into())
            .await
            .unwrap();
        runtime.release(1);
        tokio::time::timeout(Duration::from_secs(2), async {
            while admission.updates.borrow_and_update().job.state != MiningState::Syncing {
                admission.updates.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        assert!(matches!(
            jobs.start(runtime.clone(), 1, "other-key".into()).await,
            Err(MiningAdmissionError::Busy)
        ));
        runtime.sync_permits.add_permits(1);
        terminal(admission.updates).await;
    }

    #[tokio::test]
    async fn worker_panic_publishes_failure_and_releases_admission() {
        let runtime = Arc::new(FakeRuntime {
            panic_call: 2,
            ..FakeRuntime::blocked()
        });
        let jobs = Arc::new(MiningCoordinator::new());
        let admission = jobs
            .start(runtime.clone(), 3, "panic-key".into())
            .await
            .unwrap();
        runtime.release(3);
        let failed = terminal(admission.updates).await.job;
        assert_eq!(failed.state, MiningState::Failed);
        assert_eq!(failed.completed_blocks, 1);
        assert!(failed.progress_uncertain);
        assert!(failed.error.unwrap().contains("worker"));
        let next = jobs
            .start(runtime.clone(), 1, "after-panic".into())
            .await
            .unwrap();
        assert_eq!(
            terminal(next.updates).await.job.state,
            MiningState::Completed
        );
        assert_eq!(runtime.calls(), 3);
    }
}
