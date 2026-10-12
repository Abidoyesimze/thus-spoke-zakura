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

#[cfg(test)]
#[path = "../tests/unit/runtime/docker_lifecycle_tests.rs"]
mod docker_lifecycle_tests;
#[cfg(test)]
#[path = "../tests/unit/runtime/lifecycle_tests.rs"]
mod lifecycle_tests;
mod readiness;
mod recovery;

#[cfg(test)]
use crate::{client::*, intents::*};
mod deletion;
use deletion::*;
mod cleanup;
use cleanup::*;
mod docker;
use docker::*;
mod images;
use images::*;
mod instance;
pub(crate) use instance::InstanceName;
pub(crate) use instance::prefix;
use instance::*;
mod logs;
#[cfg(test)]
use logs::*;
mod resources;
pub(crate) use resources::container_running;
use resources::*;
mod startup;
use startup::*;

pub struct Runtime {
    root: PathBuf,
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
}

#[cfg(test)]
#[path = "../tests/unit/runtime/tests.rs"]
mod tests;
