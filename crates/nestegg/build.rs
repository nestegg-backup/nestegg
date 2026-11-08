//! Locate the built web UI for embedding (see `src/ui.rs`).
//!
//! - `ui/dist` exists: embed it and rebuild whenever it changes.
//! - otherwise: embed an empty directory and rebuild once `ui/` changes
//!   (e.g. after `npm run build` created `ui/dist`).

use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let ui = manifest.join("../../ui");
    let dist = ui.join("dist");
    println!("cargo:rerun-if-changed=build.rs");
    let folder = if dist.join("index.html").is_file() {
        println!("cargo:rerun-if-changed={}", dist.display());
        dist
    } else {
        if ui.is_dir() {
            println!("cargo:rerun-if-changed={}", ui.display());
        }
        let empty = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("empty-ui");
        std::fs::create_dir_all(&empty).expect("create empty ui dir");
        empty
    };
    println!("cargo:rustc-env=NESTEGG_UI_DIST={}", folder.display());
}
