use super::{InstanceAdapter, InstanceKind, ProcessHandle};
use anyhow::{anyhow, Context, Result};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Clone)]
pub struct InstanceRuntime {
    state: Arc<RwLock<RuntimeState>>,
}

#[derive(Default)]
struct RuntimeState {
    handles: HashMap<InstanceKind, ProcessHandle>,
    launching: HashSet<InstanceKind>,
}

impl InstanceRuntime {
    pub fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(RuntimeState::default())),
        }
    }

    /// Reserve a kind before asynchronous preparation so concurrent launch
    /// requests cannot replace each other's process handles.
    pub async fn reserve(&self, kind: InstanceKind) -> Result<LaunchReservation> {
        let mut state = self.state.write().await;
        if state.handles.contains_key(&kind) {
            return Err(anyhow!("{} instance is already running", kind.as_str()));
        }
        if !state.launching.insert(kind) {
            return Err(anyhow!(
                "{} instance launch is already in progress",
                kind.as_str()
            ));
        }
        Ok(LaunchReservation {
            runtime: self.clone(),
            kind,
        })
    }

    pub async fn commit(&self, reservation: LaunchReservation, handle: ProcessHandle) {
        let mut state = self.state.write().await;
        state.launching.remove(&reservation.kind);
        state.handles.insert(reservation.kind, handle);
    }

    pub async fn abort(&self, reservation: LaunchReservation) {
        let mut state = self.state.write().await;
        state.launching.remove(&reservation.kind);
    }

    pub async fn is_running(&self, kind: InstanceKind) -> bool {
        self.state.read().await.handles.contains_key(&kind)
    }

    pub async fn pid_all(&self) -> HashMap<InstanceKind, Option<u32>> {
        self.state
            .read()
            .await
            .handles
            .iter()
            .map(|(kind, handle)| (*kind, Some(handle.root_pid)))
            .collect()
    }

    pub async fn handles(&self) -> HashMap<InstanceKind, ProcessHandle> {
        self.state.read().await.handles.clone()
    }

    /// Remove the handle only after the adapter has stopped its process tree.
    /// A failed stop leaves the handle available for a later retry.
    pub async fn stop(
        &self,
        kind: InstanceKind,
        adapter: &Arc<dyn InstanceAdapter>,
    ) -> Result<bool> {
        let handle = self.state.read().await.handles.get(&kind).cloned();
        let Some(handle) = handle else {
            return Ok(false);
        };

        adapter
            .stop(&handle)
            .await
            .with_context(|| format!("failed to stop {} instance", kind.as_str()))?;

        let mut state = self.state.write().await;
        if state
            .handles
            .get(&kind)
            .map(|current| current.root_pid == handle.root_pid)
            .unwrap_or(false)
        {
            state.handles.remove(&kind);
        }
        Ok(true)
    }
}

pub struct LaunchReservation {
    runtime: InstanceRuntime,
    kind: InstanceKind,
}

impl LaunchReservation {
    pub async fn abort(self) {
        let runtime = self.runtime.clone();
        runtime.abort(self).await;
    }
}

/// Owns a spawned process until post-launch work has completed. Rollback
/// delegates to the adapter so per-launch resources such as Web session
/// directories are removed as well as the process tree.
pub struct LaunchTransaction {
    adapter: Arc<dyn InstanceAdapter>,
    handle: Option<ProcessHandle>,
}

impl LaunchTransaction {
    pub fn new(adapter: Arc<dyn InstanceAdapter>, handle: ProcessHandle) -> Self {
        Self {
            adapter,
            handle: Some(handle),
        }
    }

    pub fn handle(&self) -> Option<&ProcessHandle> {
        self.handle.as_ref()
    }

    pub fn commit(mut self) -> ProcessHandle {
        self.handle
            .take()
            .expect("launch transaction must own a process before commit")
    }

    pub async fn rollback(&mut self) -> Result<()> {
        let Some(handle) = self.handle.take() else {
            return Ok(());
        };
        let pids = self.adapter.process_tree(&handle).await;
        let stop_result = self.adapter.stop(&handle).await;
        // Adapters own normal cleanup (including Web session markers),
        // while this best-effort pass covers helpers that detach during stop.
        // Invalid test or stale PIDs are filtered before converting to libc
        // process IDs.
        terminate_pids(&pids);
        stop_result.context("adapter failed while rolling back instance launch")
    }
}

fn terminate_pids(pids: &[u32]) {
    #[cfg(unix)]
    {
        use nix::sys::signal::{kill, Signal};
        use nix::unistd::Pid;
        for pid in pids {
            if *pid == 0 || *pid > i32::MAX as u32 {
                continue;
            }
            let _ = kill(Pid::from_raw(*pid as i32), Signal::SIGTERM);
        }
        for pid in pids {
            if *pid == 0 || *pid > i32::MAX as u32 {
                continue;
            }
            let _ = kill(Pid::from_raw(*pid as i32), Signal::SIGKILL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{InstanceRuntime, LaunchTransaction};
    use crate::instance::{CapabilityReport, InstanceAdapter, InstanceKind, ProcessHandle};
    use anyhow::Result;
    use async_trait::async_trait;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    struct MockAdapter {
        stop_calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl InstanceAdapter for MockAdapter {
        async fn detect(&self) -> CapabilityReport {
            CapabilityReport {
                installed: true,
                version: None,
                path: Some("mock".to_string()),
            }
        }

        async fn prepare(&self) -> Result<()> {
            Ok(())
        }

        async fn launch(&self) -> Result<ProcessHandle> {
            Ok(ProcessHandle { root_pid: 999_999 })
        }

        async fn stop(&self, _handle: &ProcessHandle) -> Result<()> {
            self.stop_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn process_tree(&self, handle: &ProcessHandle) -> Vec<u32> {
            vec![handle.root_pid]
        }
    }

    #[tokio::test]
    async fn reserve_rejects_duplicate_and_releases_after_abort() {
        let runtime = InstanceRuntime::new();
        let reservation = runtime.reserve(InstanceKind::Web).await.unwrap();
        assert!(runtime.reserve(InstanceKind::Web).await.is_err());
        reservation.abort().await;
        assert!(runtime.reserve(InstanceKind::Web).await.is_ok());
    }

    #[tokio::test]
    async fn committed_handle_blocks_duplicate_launch() {
        let runtime = InstanceRuntime::new();
        let reservation = runtime.reserve(InstanceKind::Code).await.unwrap();
        runtime
            .commit(reservation, ProcessHandle { root_pid: 999_999 })
            .await;

        assert!(runtime.reserve(InstanceKind::Code).await.is_err());
        assert_eq!(runtime.pid_all().await[&InstanceKind::Code], Some(999_999));
    }

    #[tokio::test]
    async fn launch_transaction_rollback_stops_owned_process_once() {
        let stop_calls = Arc::new(AtomicUsize::new(0));
        let adapter = Arc::new(MockAdapter {
            stop_calls: stop_calls.clone(),
        });
        let mut transaction = LaunchTransaction::new(adapter, ProcessHandle { root_pid: 999_999 });

        assert!(transaction.handle().is_some());
        transaction.rollback().await.unwrap();
        assert!(transaction.handle().is_none());
        assert_eq!(stop_calls.load(Ordering::SeqCst), 1);

        // A second rollback is a no-op because ownership was consumed by the
        // first cleanup pass.
        transaction.rollback().await.unwrap();
        assert_eq!(stop_calls.load(Ordering::SeqCst), 1);
    }
}
