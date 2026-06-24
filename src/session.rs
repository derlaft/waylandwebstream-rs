// Lazily starts the session's client app (the `-- <command> [args...]`
// passed on the command line) inside the compositor's Wayland display.
// "Lazy" means the process isn't spawned at server startup -- only once the
// first browser connection (`/ws` or `/stream`) arrives, so an idle server
// with nobody watching never pays for running it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::{oneshot, watch, Mutex};
use tokio::task::JoinHandle;
use tracing::{info, warn};

#[derive(Clone)]
pub struct SessionManager {
    inner: Arc<Inner>,
}

struct Inner {
    command: Vec<String>,
    display_name: String,
    started: AtomicBool,
    shutdown_tx: watch::Sender<bool>,
    kill_tx: Mutex<Option<oneshot::Sender<()>>>,
    watcher: Mutex<Option<JoinHandle<()>>>,
}

impl SessionManager {
    /// `command` empty means no session app is configured -- `ensure_started`
    /// is then a permanent no-op, preserving the old behavior of expecting
    /// Wayland clients to be launched manually. `shutdown_tx` is the
    /// server's own shutdown signal: if the session command exits on its
    /// own (the user closed the app), there's nothing left to stream, so we
    /// tear the whole server down the same way Ctrl+C/SIGTERM would.
    pub fn new(command: Vec<String>, display_name: String, shutdown_tx: watch::Sender<bool>) -> Self {
        Self {
            inner: Arc::new(Inner {
                command,
                display_name,
                started: AtomicBool::new(false),
                shutdown_tx,
                kill_tx: Mutex::new(None),
                watcher: Mutex::new(None),
            }),
        }
    }

    /// Spawns the configured command on the first call; every later call
    /// (from the next connection, or a concurrent one racing this one) is a
    /// no-op. Spawn failures reset the flag so the next connection gets to
    /// retry instead of the session being permanently wedged.
    pub async fn ensure_started(&self) {
        if self.inner.command.is_empty() {
            return;
        }
        if self.inner.started.swap(true, Ordering::SeqCst) {
            return;
        }

        let program = &self.inner.command[0];
        let args = &self.inner.command[1..];
        info!("First connection established, starting session command: {} {:?}", program, args);

        match Command::new(program)
            .args(args)
            .env("WAYLAND_DISPLAY", &self.inner.display_name)
            .kill_on_drop(true)
            .spawn()
        {
            Ok(mut child) => {
                // Race the child's own exit against an explicit kill request
                // from `shutdown()`. Whichever happens first decides whether
                // we're the ones tearing the server down or just reaping a
                // process we killed ourselves.
                let (kill_tx, kill_rx) = oneshot::channel();
                *self.inner.kill_tx.lock().await = Some(kill_tx);
                let shutdown_tx = self.inner.shutdown_tx.clone();
                let handle = tokio::spawn(async move {
                    tokio::select! {
                        status = child.wait() => {
                            match status {
                                Ok(status) => info!("Session command exited ({status}), shutting down server"),
                                Err(e) => warn!("Failed to wait on session command: {e}, shutting down server"),
                            }
                            let _ = shutdown_tx.send(true);
                        }
                        _ = kill_rx => {
                            let _ = child.kill().await;
                        }
                    }
                });
                *self.inner.watcher.lock().await = Some(handle);
            }
            Err(e) => {
                warn!("Failed to start session command {:?}: {}", self.inner.command, e);
                self.inner.started.store(false, Ordering::SeqCst);
            }
        }
    }

    /// Kills the session's child process, if one was ever started, and waits
    /// for the watcher task to reap it. Called during graceful shutdown so
    /// the child doesn't outlive the server.
    pub async fn shutdown(&self) {
        if let Some(kill_tx) = self.inner.kill_tx.lock().await.take() {
            let _ = kill_tx.send(());
        }
        if let Some(handle) = self.inner.watcher.lock().await.take() {
            let _ = handle.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn child_exiting_on_its_own_triggers_shutdown() {
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let session = SessionManager::new(
            vec!["sh".to_string(), "-c".to_string(), "exit 0".to_string()],
            "wayland-test".to_string(),
            shutdown_tx,
        );

        session.ensure_started().await;

        tokio::time::timeout(Duration::from_secs(5), shutdown_rx.changed())
            .await
            .expect("shutdown signal should fire once the child exits")
            .unwrap();
        assert!(*shutdown_rx.borrow());
    }

    #[tokio::test]
    async fn explicit_shutdown_kills_child_without_signaling_shutdown() {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let session = SessionManager::new(
            vec!["sleep".to_string(), "30".to_string()],
            "wayland-test".to_string(),
            shutdown_tx,
        );

        session.ensure_started().await;
        tokio::time::timeout(Duration::from_secs(5), session.shutdown())
            .await
            .expect("shutdown should kill the child promptly");

        // Our own kill shouldn't be mistaken for the child exiting on its
        // own -- the shutdown signal stays whatever the caller set it to.
        assert!(!*shutdown_rx.borrow());
    }
}
