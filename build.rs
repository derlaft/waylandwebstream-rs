// Builds the `web/` Vite+Svelte client into `web/dist/` so `src/web/mod.rs`
// can embed it with `rust-embed`. Cargo only reruns this when the watched
// paths below change, so a plain `cargo build` with no front-end changes
// doesn't pay the `npm` cost.
use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=web/src");
    println!("cargo:rerun-if-changed=web/index.html");
    println!("cargo:rerun-if-changed=web/package.json");
    println!("cargo:rerun-if-changed=web/package-lock.json");
    println!("cargo:rerun-if-changed=web/vite.config.ts");
    println!("cargo:rerun-if-changed=web/svelte.config.js");
    println!("cargo:rerun-if-changed=web/tsconfig.json");

    let web_dir = Path::new("web");
    let dist_dir = web_dir.join("dist");

    if Command::new("npm").arg("--version").output().is_err() {
        if dist_dir.join("index.html").exists() {
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
