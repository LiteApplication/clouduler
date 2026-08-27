//! Picks the directory `ui.rs` embeds via rust-embed. Uses the real `ui/dist`
//! build when it exists; otherwise points at a placeholder under OUT_DIR so
//! the crate still compiles with no ui build available.
//!
//! If `ui/node_modules` already exists, runs `npm run build` in `ui/` to
//! (re)produce `ui/dist` before embedding it. This is gated on
//! `node_modules` rather than just npm's presence: GitHub-hosted runners
//! (and the dev machine) always have npm on PATH, but CI's rust/examples
//! jobs and the aarch64 cross-build recipe never run `npm ci` in `ui/`, and
//! must keep working off a `ui/dist` built separately (or fall back to the
//! placeholder) instead of failing on missing devDependencies.

use std::path::Path;
use std::process::Command;

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("set by cargo");
    let out_dir = std::env::var("OUT_DIR").expect("set by cargo");

    let ui_dir = Path::new(&manifest_dir).join("../../ui");
    let real_dist = ui_dir.join("dist");

    println!("cargo:rerun-if-changed={}", ui_dir.join("src").display());
    println!("cargo:rerun-if-changed={}", ui_dir.join("package.json").display());

    if ui_dir.join("node_modules").is_dir() && which_npm_on_path() {
        let status = Command::new("npm")
            .args(["--prefix", ui_dir.to_str().expect("utf8 path"), "run", "build"])
            .status()
            .expect("failed to execute npm build");

        if !status.success() {
            panic!("UI build failed");
        }
    }

    let embed_dir = if real_dist.join("index.html").is_file() {
        real_dist.clone()
    } else {
        let placeholder = Path::new(&out_dir).join("ui_placeholder");
        std::fs::create_dir_all(&placeholder).expect("create placeholder ui dir");
        std::fs::write(
            placeholder.join("index.html"),
            include_str!("src/ui_placeholder.html"),
        )
        .expect("write placeholder index.html");
        placeholder
    };

    println!(
        "cargo:rustc-env=CL_API_UI_DIR={}",
        embed_dir.canonicalize().expect("canonicalize embed dir").display()
    );
}

fn which_npm_on_path() -> bool {
    Command::new("npm")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success())
}
