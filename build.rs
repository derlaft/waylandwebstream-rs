// Builds the `web/` Vite+Svelte client into `web/dist/` so `src/web/mod.rs`
// can embed it with `rust-embed`. Cargo only reruns this when the watched
// paths below change, so a plain `cargo build` with no front-end changes
// doesn't pay the `npm` cost.
use std::path::Path;
use std::process::Command;
use std::time::SystemTime;

const WATCHED_PATHS: &[&str] = &[
    "src",
    "index.html",
    "package.json",
    "package-lock.json",
    "vite.config.ts",
    "svelte.config.js",
    "tsconfig.json",
];

fn main() {
    for path in WATCHED_PATHS {
        println!("cargo:rerun-if-changed=web/{path}");
    }

    let web_dir = Path::new("web");
    let dist_index = web_dir.join("dist").join("index.html");

    // `cargo:rerun-if-changed` only dedups reruns within one fingerprint
    // bucket -- a plain `cargo build` and e.g. rust-analyzer's
    // `cargo check --all-targets` get *different* buckets (different flags),
    // so each one's first-ever invocation of this script still pays the
    // full `npm ci` cost on a freshly checked-out repo, even back-to-back.
    // Checking dist's freshness against the actual filesystem -- instead of
    // only trusting "is this the first time *this* fingerprint ran" --
    // makes every invocation after the very first one a no-op, regardless
    // of which Cargo command triggers it.
    if let Some(dist_mtime) = modified(&dist_index) {
        let stale = WATCHED_PATHS
            .iter()
            .any(|p| is_newer_than(&web_dir.join(p), dist_mtime));
        if !stale {
            return;
        }
    }

    if Command::new("npm").arg("--version").output().is_err() {
        if dist_index.exists() {
            println!(
                "cargo:warning=npm not found; reusing existing web/dist (may be stale)"
            );
            return;
        }
        panic!(
            "npm not found on PATH and no prebuilt web/dist/ exists. \
             Install Node.js/npm to build the web client, or commit a prebuilt web/dist/."
        );
    }

    run(web_dir, "npm", &["ci"]);
    run(web_dir, "npm", &["run", "build"]);
}

fn modified(path: &Path) -> Option<SystemTime> {
    path.metadata().ok()?.modified().ok()
}

/// True if `path` (or, for a directory, anything anywhere under it) has a
/// modification time after `since`. Used to tell whether `web/dist` is
/// actually stale relative to its inputs, rather than relying on Cargo's
/// per-fingerprint rerun tracking (see the comment in `main`).
fn is_newer_than(path: &Path, since: SystemTime) -> bool {
    match modified(path) {
        Some(t) if t > since => return true,
        Some(_) => {}
        None => return false,
    }
    if path.is_dir() {
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                if is_newer_than(&entry.path(), since) {
                    return true;
                }
            }
        }
    }
    false
}

fn run(dir: &Path, cmd: &str, args: &[&str]) {
    let status = Command::new(cmd)
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap_or_else(|e| panic!("failed to spawn `{cmd} {}`: {e}", args.join(" ")));
    if !status.success() {
        panic!("`{cmd} {}` exited with {status}", args.join(" "));
    }
}
