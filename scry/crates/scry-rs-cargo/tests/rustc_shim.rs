//! `cargo scry dx` runs a copy of `cargo-scry` named `rustc`, because dx runs `rustc` by name for
//! the crates it compiles. These tests run such a copy the way dx would.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A copy of `cargo-scry` named `rustc`, in a folder of its own.
fn shim(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let shim = dir.join(format!("rustc{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(env!("CARGO_BIN_EXE_cargo-scry"), &shim).unwrap();
    shim
}

fn real_rustc() -> PathBuf {
    let output = Command::new("rustc").args(["--print", "sysroot"]).output().unwrap();
    let sysroot = String::from_utf8(output.stdout).unwrap();
    PathBuf::from(sysroot.trim()).join("bin").join(format!("rustc{}", std::env::consts::EXE_SUFFIX))
}

fn text(output: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr))
}

#[test]
fn a_copy_named_rustc_runs_the_real_rustc() {
    let dir = std::env::temp_dir().join(format!("scry-shim-test-{}", std::process::id()));
    let shim = shim(&dir);

    let probe = Command::new(&shim).arg("-vV").env("SCRY_REAL_RUSTC", real_rustc()).output().unwrap();
    assert!(probe.status.success(), "{}", text(&probe));
    assert!(text(&probe).contains("host:"), "{}", text(&probe));

    // A crate that hasn't opted in compiles unchanged.
    let source = dir.join("plain.rs");
    std::fs::write(&source, "pub fn answer() -> u32 { 42 }\n").unwrap();
    let build = Command::new(&shim)
        .args(["--crate-name", "plain", "--crate-type", "lib", "--edition", "2021", "--out-dir"])
        .arg(&dir)
        .arg(&source)
        .env("SCRY_REAL_RUSTC", real_rustc())
        .output()
        .unwrap();
    assert!(build.status.success(), "{}", text(&build));
    assert!(dir.join("libplain.rlib").is_file());

    let unset = Command::new(&shim).arg("-vV").env_remove("SCRY_REAL_RUSTC").output().unwrap();
    assert!(!unset.status.success());
    assert!(text(&unset).contains("SCRY_REAL_RUSTC"), "{}", text(&unset));

    let _ = std::fs::remove_dir_all(&dir);
}
