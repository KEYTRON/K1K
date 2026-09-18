//! Builds the ring-3 services (a separate Cargo workspace in `../user`) and
//! copies their ELF images into OUT_DIR so the kernel can embed them.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

const SERVICES: &[&str] = &["hello", "flaky", "ping", "pong", "kbd", "blk"];

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let user_dir = manifest.join("../user").canonicalize().unwrap();
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());

    println!("cargo:rerun-if-changed=linker.ld");
    for p in [
        "Cargo.toml",
        "Cargo.lock",
        "user.ld",
        ".cargo/config.toml",
        "rt",
    ] {
        println!("cargo:rerun-if-changed={}", user_dir.join(p).display());
    }
    for s in SERVICES {
        println!(
            "cargo:rerun-if-changed={}",
            user_dir.join("svc").join(s).display()
        );
    }

    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = Command::new(cargo)
        .current_dir(&user_dir)
        .args(["build", "--release", "--workspace"])
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("CARGO_BUILD_RUSTFLAGS")
        .env_remove("CARGO_BUILD_TARGET")
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("CARGO_BUILD_TARGET_DIR")
        .env_remove("CARGO_UNSTABLE_BUILD_STD")
        .env_remove("CARGO_MAKEFLAGS")
        .env_remove("MAKEFLAGS")
        .env_remove("MFLAGS")
        .status()
        .expect("failed to spawn cargo for the user workspace");
    assert!(status.success(), "building user services failed");

    let bin_dir = user_dir.join("target/x86_64-unknown-none/release");
    for s in SERVICES {
        let src = bin_dir.join(s);
        let dst = out.join(format!("{s}.elf"));
        std::fs::copy(&src, &dst).unwrap_or_else(|e| panic!("copy {}: {e}", src.display()));
    }

    let _ = Path::new(&out);
}
