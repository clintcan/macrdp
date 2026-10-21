// Bake an rpath to the Xcode Swift runtime into the final binary.
//
// The `screencapturekit` crate's own build.rs emits the same rpath flags,
// but `cargo:rustc-link-arg` is scoped to the emitting package only — so a
// lib crate's flags never reach a downstream binary's link command. We have
// to re-emit them here, where the package being linked IS the binary.
//
// Symptom if you drop this: `dyld: Library not loaded: @rpath/libswift_Concurrency.dylib`.

use std::process::Command;

fn main() {
    git_describe();
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }

    // Compile the USB-redirection Obj-C shim (Phase-1b UserHCI spike) and link
    // the public IOUSBHost framework. macOS-only; the module's non-macOS stub
    // never references the extern, so Linux CI builds without any of this.
    println!("cargo:rerun-if-changed=src/usb_redirect/usb_spike.m");
    cc::Build::new()
        .file("src/usb_redirect/usb_spike.m")
        .flag("-fobjc-arc")
        .compile("macrdp_usb_spike");
    println!("cargo:rustc-link-lib=framework=IOUSBHost");
    println!("cargo:rustc-link-lib=framework=Foundation");

    // /usr/lib/swift holds most of Swift's stdlib on modern macOS — cheap to
    // include even though libswift_Concurrency.dylib lives only in the Xcode
    // toolchain on this version of macOS.
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");

    let xcode_dev_dir = match Command::new("xcode-select").arg("-p").output() {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        _ => {
            println!(
                "cargo:warning=xcode-select -p failed; macrdp may not load at runtime. \
                 Install full Xcode (not just Command Line Tools)."
            );
            return;
        }
    };

    // Both the old swift-5.5 layout and the unversioned `swift/` layout — Xcode
    // versions differ on which exists. Adding both rpaths is harmless if one
    // is absent; the dynamic linker just skips missing entries.
    for slice in ["swift-5.5", "swift"] {
        let path =
            format!("{xcode_dev_dir}/Toolchains/XcodeDefault.xctoolchain/usr/lib/{slice}/macosx");
        println!("cargo:rustc-link-arg=-Wl,-rpath,{path}");
    }
}

/// `MACRDP_GIT_DESCRIBE` for `--version` and the startup log (see
/// `src/version.rs`): `git describe` output, or empty without git.
fn git_describe() {
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    // Rebuild when HEAD, the refs or the index change, so describe stays
    // current. Asks git for the paths: in a worktree, `.git` is a file. Only
    // paths that exist: a missing path would rebuild every time.
    if let (Some(dir), Some(common)) = (
        git(&["rev-parse", "--git-dir"]),
        git(&["rev-parse", "--git-common-dir"]),
    ) {
        let paths = [
            format!("{dir}/HEAD"),
            format!("{dir}/index"),
            format!("{common}/refs"),
            format!("{common}/packed-refs"),
        ];
        for path in paths.iter().filter(|p| std::path::Path::new(p).exists()) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    // No `--dirty`: an edited file does not rerun this script, so it would
    // show a stale state.
    let describe = git(&["describe", "--tags", "--always"]).unwrap_or_default();
    println!("cargo:rustc-env=MACRDP_GIT_DESCRIBE={describe}");
}
