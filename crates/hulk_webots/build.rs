//! Lets the binary find Webots' libController.dylib at runtime: next to an app bundle
//! (`Resources/hulk/` beside `Resources/Webots.app`) or in the Webots install used for building.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        let home = std::env::var("WEBOTS_HOME").unwrap_or_else(|_| "/Applications/Webots.app".into());
        println!("cargo:rerun-if-env-changed=WEBOTS_HOME");
        println!("cargo:rustc-link-arg=-Wl,-rpath,@executable_path/../Webots.app");
        println!("cargo:rustc-link-arg=-Wl,-rpath,{home}");
    }
}
