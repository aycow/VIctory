//! Enable the optional app-local D3D12 runtime for the MSVC unit tests.

use std::path::PathBuf;

const AGILITY: &str = "VICTORY_TEST_AGILITY";

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-env-changed={AGILITY}");
    println!("cargo::rustc-check-cfg=cfg(victory_test_agility)");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows")
        || std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc")
    {
        return;
    }

    // OUT_DIR is <target>/<profile>/build/<package>-<hash>/out.
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR"));
    let profile = out.ancestors().nth(3).expect("Cargo profile directory");
    let core = profile.join("D3D12").join("D3D12Core.dll");
    let enabled = match std::env::var(AGILITY).as_deref() {
        Ok("1") => {
            assert!(
                core.is_file(),
                "stage Agility SDK 1.620.2 at {} before enabling {AGILITY}",
                core.display()
            );
            true
        }
        Ok("0") => false,
        Err(std::env::VarError::NotPresent) => core.is_file(),
        _ => panic!("{AGILITY} must be 0 or 1"),
    };
    if enabled {
        // Watching an absent file would rerun this script on every build.
        // The tracked environment variable enables staging after a build.
        println!("cargo::rerun-if-changed={}", core.display());
        println!("cargo::rustc-cfg=victory_test_agility");
        // The exported definitions are only present in the unit test binary.
        println!("cargo::rustc-link-arg=/EXPORT:D3D12SDKVersion,DATA");
        println!("cargo::rustc-link-arg=/EXPORT:D3D12SDKPath,DATA");
    }
}
