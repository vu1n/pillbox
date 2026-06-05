//! Build script. The only thing here is the optional libkrun link wiring, emitted
//! **only** when the `libkrun` feature is enabled — so the default build (and any
//! dev/CI without libkrun installed) is completely unaffected.
//!
//! libkrun is cross-platform (Linux/KVM + macOS/HVF) and the `krun` C API is
//! identical on both; only *where the lib lives* differs. We link `krun` and embed
//! an rpath to the dir that holds it (libkrun `dlopen`s a bare `libkrunfw.*` that
//! the loader resolves via the binary's rpath, not LD_LIBRARY_PATH/DYLD). The dir
//! is `$LIBKRUN_LIB_DIR` if set (nonstandard install prefixes), else a per-OS
//! default: Homebrew on macOS, `make install`'s `/usr/local/lib` on Linux.

fn main() {
    if std::env::var_os("CARGO_FEATURE_LIBKRUN").is_none() {
        return;
    }
    println!("cargo:rerun-if-env-changed=LIBKRUN_LIB_DIR");
    println!("cargo:rustc-link-lib=dylib=krun");

    let lib_dir = std::env::var("LIBKRUN_LIB_DIR").unwrap_or_else(|_| {
        match std::env::var("CARGO_CFG_TARGET_OS").as_deref() {
            Ok("macos") => "/opt/homebrew/lib".to_string(), // slp/krun Homebrew tap
            _ => "/usr/local/lib".to_string(),              // libkrun `make install` default on Linux
        }
    });
    println!("cargo:rustc-link-search=native={lib_dir}");
    println!("cargo:rustc-link-arg=-Wl,-rpath,{lib_dir}");
}
