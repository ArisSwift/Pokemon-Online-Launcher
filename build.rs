use std::path::Path;

fn main() {
    // Only Linux needs the custom link order below. Windows and macOS let cef-dll-sys
    // link the wrapper and copy or load the runtime itself.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        return;
    }

    // Chromium's close() interposer in base/files/scoped_file_linux.cc resolves the
    // real libc close with dlsym(RTLD_NEXT, "close") and ImmediateCrash()es when it
    // gets NULL. RTLD_NEXT only searches objects loaded *after* libcef.so, so libcef
    // has to come before libc in the loader's global scope. The cef crate dlopen()s
    // libcef on Linux, which puts it last every time and crashes inside
    // cef_initialize. Linking it as a normal dependency instead makes it a DT_NEEDED
    // entry ahead of libc, which is the load order the interposer expects.
    let out = std::env::var("OUT_DIR").expect("OUT_DIR");
    // <target>/<profile>/build/<pkg>-<hash>/out -> <target>/<profile>, where cef-dll-sys
    // copies the runtime libraries next to the binary.
    let bin_dir = Path::new(&out)
        .ancestors()
        .nth(3)
        .expect("binary directory")
        .to_path_buf();

    println!("cargo:rustc-link-search=native={}", bin_dir.display());
    println!("cargo:rustc-link-lib=dylib=cef");
    // Find libcef.so beside the executable at runtime, wherever the app is unpacked.
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");
}
