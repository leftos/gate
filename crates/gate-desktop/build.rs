//! Links `gate-desktop-helper.exe` to load its static imports from System32 only (`LOAD_LIBRARY_SEARCH_SYSTEM32`), so
//! a DLL planted beside the installed exe is never loaded in place of a system one, and fails a build that lost the
//! static C runtime.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=RUSTFLAGS");
    println!("cargo:rerun-if-env-changed=CARGO_ENCODED_RUSTFLAGS");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }
    let features = std::env::var("CARGO_CFG_TARGET_FEATURE").unwrap_or_default();
    if !features.split(',').any(|feature| feature == "crt-static") {
        println!(
            "cargo::error=gate's exes must link the C runtime statically (.cargo/config.toml); RUSTFLAGS or CARGO_ENCODED_RUSTFLAGS overrides it, so unset them for this build"
        );
    }
    println!("cargo:rustc-link-arg-bin=gate-desktop-helper=/DEPENDENTLOADFLAG:0x800");
}
