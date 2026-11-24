//! Wraps the built binary in the macOS app bundle CEF expects.
//!
//! Chromium cannot run from a bare Mach-O executable: it resolves its framework
//! and helper apps from fixed places inside `Foo.app/Contents`. `cef` ships the
//! layout code as `build_util::mac::bundle`, so this only supplies our names and
//! points it at the binaries cargo already built.
//!
//! Usage: `bundle-macos [profile] [output dir]`, defaults `release` and `dist`.

#[cfg(target_os = "macos")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use cef::build_util::mac::{bundle, BundleInfo};
    use std::path::PathBuf;

    let mut args = std::env::args().skip(1);
    let profile = args.next().unwrap_or_else(|| String::from("release"));
    let out_dir = PathBuf::from(args.next().unwrap_or_else(|| String::from("dist")));
    let target_dir = PathBuf::from(
        std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| String::from("target")),
    );

    let info = BundleInfo {
        name: String::from("Pokem Online"),
        identifier: String::from("onl.pokem.launcher"),
        display_name: String::from("Pokem Online"),
        development_region: String::from("English"),
        version: semver::Version::parse(env!("CARGO_PKG_VERSION"))?,
    };

    // The helper apps are the same binary: CEF tells them apart with `--type`,
    // and main() switches to the framework path those deeper bundles need.
    let app = bundle(
        &out_dir,
        &target_dir.join(&profile),
        "pokem-online",
        "pokem-online",
        None,
        info,
    )?;
    println!("{}", app.display());
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("bundle-macos only runs on macOS");
}
