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

    // We only ever invoke npm automatically when `web/dist` doesn't exist at
    // all (e.g. a fresh checkout). Once it exists, we never rebuild it for
    // the caller automatically -- we just warn if it looks stale. Auto-
    // rebuilding on every detected staleness used to retrigger `npm ci` +
    // `npm run build` repeatedly whenever something (e.g. rust-analyzer
    // firing `cargo check` on every save) invoked this script in quick
    // succession, since each npm run's own filesystem writes could make the
    // *next* invocation's staleness check fire again. Building at most once
    // per checkout removes that loop entirely.
    if dist_index.exists() {
        if let Some(dist_mtime) = modified(&dist_index) {
            let stale = WATCHED_PATHS
                .iter()
                .any(|p| is_newer_than(&web_dir.join(p), dist_mtime));
            if stale {
                println!(
                    "cargo:warning=web/dist looks older than the frontend sources; \
                     rebuild it manually with `cd web && npm run build` (or `npm ci && npm run build`) \
                     if you've changed the web client."
                );
            }
        }
        return;
    }

    if Command::new("npm").arg("--version").output().is_err() {
        panic!(
            "npm not found on PATH and no prebuilt web/dist/ exists. \
             Install Node.js/npm and run `cd web && npm ci && npm run build`, \
             or commit a prebuilt web/dist/."
        );
    }

    println!("cargo:warning=web/dist not found; running `npm ci && npm run build` once");
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
