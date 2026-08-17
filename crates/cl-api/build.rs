//! Picks the directory `ui.rs` embeds via rust-embed. Uses the real `ui/dist`
//! build when it exists; otherwise points at a placeholder under OUT_DIR so
//! the crate still compiles with no ui build available.

use std::path::Path;

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("set by cargo");
    let out_dir = std::env::var("OUT_DIR").expect("set by cargo");

    let real_dist = Path::new(&manifest_dir).join("../../ui/dist");
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
    println!("cargo:rerun-if-changed={}", real_dist.display());
}
