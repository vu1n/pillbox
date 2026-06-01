//! Build script. The only thing here is the optional libkrun link wiring, emitted
//! **only** when the `libkrun` feature is enabled — so the default build (and any
//! dev/CI without libkrun installed) is completely unaffected.

fn main() {
    if std::env::var_os("CARGO_FEATURE_LIBKRUN").is_some() {
        // libkrun + libkrunfw from the slp/krun homebrew tap. libkrun `dlopen`s a
        // bare `libkrunfw.*.dylib` that macOS resolves against the main binary's
        // LC_RPATH (not DYLD_LIBRARY_PATH), so we add the rpath here too.
        println!("cargo:rustc-link-search=native=/opt/homebrew/lib");
        println!("cargo:rustc-link-lib=dylib=krun");
        println!("cargo:rustc-link-arg=-Wl,-rpath,/opt/homebrew/lib");
    }
}
