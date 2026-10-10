//! Picks the directory `ui.rs` embeds via rust-embed.
//!
//! If `ui/node_modules` already exists, runs `npm run build` in `ui/` with
//! its output redirected into this build's own OUT_DIR and embeds that.
//! Otherwise uses a prebuilt `ui/dist` when it has an `index.html`, or a
//! placeholder page so the crate still compiles with no ui build available.
//!
//! The npm build is gated on
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
    println!(
        "cargo:rerun-if-changed={}",
        ui_dir.join("package.json").display()
    );

    for watched in ["index.html", "public", "vite.config.ts"] {
        println!("cargo:rerun-if-changed={}", ui_dir.join(watched).display());
    }

    let embed_dir = if ui_dir.join("node_modules").is_dir() && which_npm_on_path() {
        // Built into this build's own OUT_DIR, never the shared `ui/dist`:
        // vite empties its output directory first, so two cargo builds running
        // at once (one per target in a cross-build) used to wipe `ui/dist`
        // under each other, and one binary shipped with the icons but no
        // `index.html` or JS, serving a bare "not found" for the dashboard.
        let own_dist = Path::new(&out_dir).join("ui_dist");
        let status = Command::new("npm")
            .args([
                "--prefix",
                ui_dir.to_str().expect("utf8 path"),
                "run",
                "build",
                "--",
                "--outDir",
                own_dist.to_str().expect("utf8 path"),
                "--emptyOutDir",
            ])
            .status()
            .expect("failed to execute npm build");

        if !status.success() {
            panic!("UI build failed");
        }
        if !own_dist.join("index.html").is_file() {
            panic!("UI build produced no index.html in {}", own_dist.display());
        }
        own_dist
    } else if real_dist.join("index.html").is_file() {
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
        embed_dir
            .canonicalize()
            .expect("canonicalize embed dir")
            .display()
    );
}

fn which_npm_on_path() -> bool {
    Command::new("npm")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success())
}
