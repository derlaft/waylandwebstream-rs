// Lazily starts the session's client app (the `-- <command> [args...]`
// passed on the command line) inside the compositor's Wayland display.
// "Lazy" means the process isn't spawned at server startup -- only once the
// first browser connection (`/ws` or `/stream`) arrives, so an idle server
// with nobody watching never pays for running it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tracing::{info, warn};

#[derive(Clone)]
pub struct SessionManager {
    inner: Arc<Inner>,
}

struct Inner {
    command: Vec<String>,
    display_name: String,
    started: AtomicBool,
    child: Mutex<Option<Child>>,
}

impl SessionManager {
    /// `command` empty means no session app is configured -- `ensure_started`
    /// is then a permanent no-op, preserving the old behavior of expecting
    /// Wayland clients to be launched manually.
    pub fn new(command: Vec<String>, display_name: String) -> Self {
        Self {
            inner: Arc::new(Inner {
                command,
                display_name,
                started: AtomicBool::new(false),
                child: Mutex::new(None),
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
            Ok(child) => {
                *self.inner.child.lock().await = Some(child);
            }
            Err(e) => {
                warn!("Failed to start session command {:?}: {}", self.inner.command, e);
                self.inner.started.store(false, Ordering::SeqCst);
            }
        }
    }

    /// Kills the session's child process, if one was ever started. Called
    /// during graceful shutdown so the child doesn't outlive the server.
    pub async fn shutdown(&self) {
        if let Some(mut child) = self.inner.child.lock().await.take() {
            let _ = child.kill().await;
        }
    }
}
