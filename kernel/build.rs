use std::env;
use std::path::Path;
use std::process::Command;

const USER_PROGRAMS: &[&str] = &["hello", "flaky", "ping", "pong"];

fn main() {
    let out = env::var("OUT_DIR").unwrap();
    let user_dir = Path::new("../user");
    println!("cargo:rerun-if-changed=linker.ld");
    println!("cargo:rerun-if-changed=../user/lib/k1k.inc");

    for prog in USER_PROGRAMS {
        let src = user_dir.join(format!("{prog}.asm"));
        println!("cargo:rerun-if-changed={}", src.display());
        let status = Command::new("nasm")
            .args(["-f", "bin", "-I", "../user/lib/", "-o"])
            .arg(format!("{out}/{prog}.bin"))
            .arg(&src)
            .status()
            .expect("nasm not found — required to build user programs");
        assert!(status.success(), "nasm failed on {}", src.display());
    }
}
