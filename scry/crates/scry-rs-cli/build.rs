//! Gives `scry-rs.exe` its icon on Windows.
//!
//! `assets/scry.res` is compiled ahead of time from `assets/scry.rc`, so building needs no
//! resource compiler. The MSVC linker takes the `.res` file as it is. After changing the icon,
//! rebuild the `.res` file from the `assets` folder with the Windows SDK's `rc /fo scry.res scry.rc`.
//! The icon, `assets/scry.ico`, is drawn from the repository's `assets/scry-rs-logo.svg`.

fn main() {
    println!("cargo:rerun-if-changed=assets/scry.res");
    let windows = std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "windows");
    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").is_ok_and(|env| env == "msvc");
    if windows && msvc {
        let res = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets").join("scry.res");
        println!("cargo:rustc-link-arg-bin=scry-rs={}", res.display());
    }
}
