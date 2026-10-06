//! A build id the client and the serving S-CTRL compare: a command
//! forwarded to a daemon from another build would be parsed by other code.
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    for dir in ["src", "../core/src", "../proto/src"] {
        println!("cargo:rerun-if-changed={dir}");
    }
    let id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    println!(
        "cargo:rustc-env=SCTRL_BUILD_ID={}-{id:x}",
        std::env::var("CARGO_PKG_VERSION").unwrap_or_default()
    );
}
