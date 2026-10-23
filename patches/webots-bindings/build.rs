//! Generates bindings against an installed Webots and links its controller library, so the
//! controller always matches the simulator it talks to.
//!
//! `WEBOTS_HOME` points at the Webots install (`/Applications/Webots.app` on macOS,
//! `/usr/local/webots` on Linux).

use std::{env, path::PathBuf};

fn main() {
    let default_home = if cfg!(target_os = "macos") {
        "/Applications/Webots.app"
    } else {
        "/usr/local/webots"
    };
    let home = PathBuf::from(env::var("WEBOTS_HOME").unwrap_or_else(|_| default_home.into()));
    // macOS app bundles keep everything below Contents/.
    let root = if home.join("Contents").is_dir() {
        home.join("Contents")
    } else {
        home.clone()
    };
    let include = root.join("include/controller/c");
    let lib = root.join("lib/controller");
    assert!(
        include.join("webots/robot.h").exists(),
        "Webots headers not found below {}; set WEBOTS_HOME",
        home.display()
    );

    println!("cargo:rerun-if-env-changed=WEBOTS_HOME");
    println!("cargo:rerun-if-changed=wrapper.h");
    println!("cargo:rustc-link-search=native={}", lib.display());
    println!("cargo:rustc-link-lib=dylib=Controller");

    let bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .clang_args(["-I", include.to_str().unwrap()])
        .blocklist_item("FP_INFINITE")
        .blocklist_item("FP_NAN")
        .blocklist_item("FP_NORMAL")
        .blocklist_item("FP_SUBNORMAL")
        .blocklist_item("FP_ZERO")
        .generate()
        .expect("Failed to generate bindings");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out.join("webots_bindings.rs"))
        .expect("Failed to write bindings");
}
