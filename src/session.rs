// Lazily starts the session's client app (the `-- <command> [args...]`
// passed on the command line) inside the compositor's Wayland display.
// "Lazy" means the process isn't spawned at server startup -- only once the
// first browser connection (`/client`) arrives, so an idle server
// with nobody watching never pays for running it.

use std::collections::HashSet;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
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
    /// `WAYLAND_DISPLAY` of the *nested* compositor the session command starts
    /// (e.g. labwc), discovered after spawn by watching `$XDG_RUNTIME_DIR` for
    /// a new `wayland-*` socket. `None` until discovered (or if the session
    /// app isn't a compositor). Used by the clipboard bridge.
    nested_display_tx: watch::Sender<Option<String>>,
}

impl SessionManager {
    /// `command` empty means no session app is configured -- `ensure_started`
    /// is then a permanent no-op, preserving the old behavior of expecting
    /// Wayland clients to be launched manually. `shutdown_tx` is the
    /// server's own shutdown signal: if the session command exits on its
    /// own (the user closed the app), there's nothing left to stream, so we
    /// tear the whole server down the same way Ctrl+C/SIGTERM would.
    pub fn new(
        command: Vec<String>,
        display_name: String,
        shutdown_tx: watch::Sender<bool>,
    ) -> Self {
        let (nested_display_tx, _) = watch::channel(None);
        Self {
            inner: Arc::new(Inner {
                command,
                display_name,
                started: AtomicBool::new(false),
                shutdown_tx,
                kill_tx: Mutex::new(None),
                watcher: Mutex::new(None),
                nested_display_tx,
            }),
        }
    }

    /// Subscribe to the nested compositor's `WAYLAND_DISPLAY`, discovered once
    /// the session command (a nested compositor) has created its own socket.
    pub fn nested_display(&self) -> watch::Receiver<Option<String>> {
        self.inner.nested_display_tx.subscribe()
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
        info!(
            "First connection established, starting session command: {} {:?}",
            program, args
        );

        // Snapshot the currently *live* wayland sockets so we can spot the new
        // one the session's nested compositor creates (see
        // `discover_nested_display`).
        let before = live_wayland_displays(&self.inner.display_name);

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

                // Discover the nested compositor's socket in the background and
                // publish it for the clipboard bridge. Excludes our own display.
                let own = self.inner.display_name.clone();
                let nested_tx = self.inner.nested_display_tx.clone();
                tokio::spawn(async move {
                    if let Some(nested) = discover_nested_display(&before, &own).await {
                        info!("Discovered nested compositor display: {}", nested);
                        let _ = nested_tx.send(Some(nested));
                    } else {
                        warn!(
                            "No nested compositor socket appeared; clipboard sync \
                             will be unavailable (session app may not be a compositor)"
                        );
                    }
                });
            }
            Err(e) => {
                warn!(
                    "Failed to start session command {:?}: {}",
                    self.inner.command, e
                );
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

/// True for a wayland *display* socket name: `wayland-` followed by digits
/// (e.g. `wayland-0`). Excludes our own `wayland-wws-*`, `.lock` files, and
/// per-app proxies like `wayland-proxy-<pid>`.
fn is_wayland_display_name(name: &str) -> bool {
    name.strip_prefix("wayland-")
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
}

/// The set of *live* (connectable) wayland display sockets in
/// `$XDG_RUNTIME_DIR`, excluding our own. Connectability matters: a nested
/// compositor often reuses the conventional `wayland-0` name, and a stale
/// `wayland-0` socket *file* from a previous run would defeat a name-only diff
/// -- but a stale file isn't connectable, so it's filtered out here.
fn live_wayland_displays(own_display: &str) -> HashSet<String> {
    let dir = match std::env::var("XDG_RUNTIME_DIR") {
        Ok(d) => d,
        Err(_) => return HashSet::new(),
    };
    live_wayland_displays_in(&dir, own_display)
}

/// `live_wayland_displays` with an explicit directory, for testability.
fn live_wayland_displays_in(dir: &str, own_display: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if name == own_display || !is_wayland_display_name(&name) {
                continue;
            }
            if UnixStream::connect(format!("{dir}/{name}")).is_ok() {
                out.insert(name);
            }
        }
    }
    out
}

/// Polls for a newly *live* wayland display to appear after the session
/// compositor was spawned (one that wasn't live before, so a stale socket or a
/// pre-existing host compositor isn't mistaken for it). Returns the first new
/// one, or `None` after ~10s (the session app probably isn't a compositor).
async fn discover_nested_display(before: &HashSet<String>, own_display: &str) -> Option<String> {
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        for name in live_wayland_displays(own_display) {
            if !before.contains(&name) {
                return Some(name);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::time::Duration;

    #[test]
    fn wayland_display_name_matching() {
        assert!(is_wayland_display_name("wayland-0"));
        assert!(is_wayland_display_name("wayland-12"));
        // Our own socket, per-app proxies, lock files, and non-wayland sockets
        // must not be mistaken for a nested compositor display.
        assert!(!is_wayland_display_name("wayland-wws-0"));
        assert!(!is_wayland_display_name("wayland-proxy-845"));
        assert!(!is_wayland_display_name("wayland-0.lock"));
        assert!(!is_wayland_display_name("wayland-"));
        assert!(!is_wayland_display_name("pipewire-0"));
    }

    #[test]
    fn live_displays_ignores_stale_own_and_proxies() {
        let dir = tempfile::tempdir().unwrap();
        let path = |name: &str| dir.path().join(name);
        // A live nested-compositor socket -- the one we want to find.
        let _live = UnixListener::bind(path("wayland-0")).unwrap();
        // Our own socket, live but excluded by name.
        let _own = UnixListener::bind(path("wayland-wws-0")).unwrap();
        // A firefox-style proxy, live but excluded by the name pattern.
        let _proxy = UnixListener::bind(path("wayland-proxy-123")).unwrap();
        // A stale socket *file* with no listener -- not connectable, so it must
        // be ignored (this is the bug the connectability check fixed).
        std::fs::File::create(path("wayland-1")).unwrap();

        let live = live_wayland_displays_in(dir.path().to_str().unwrap(), "wayland-wws-0");
        assert_eq!(live, HashSet::from(["wayland-0".to_string()]));
    }

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
