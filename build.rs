//! Build script — Windows resource wiring only.
//!
//! On every other target this is a no-op, so adding it changes nothing for the
//! macOS or Linux builds.
//!
//! The Windows desktop shell needs an application manifest to look like a real
//! Windows program at all:
//!
//! * **Common-Controls v6** — without it the process links against comctl32 v5
//!   and every control (tree, progress bars, buttons, tabs) renders in unthemed
//!   Windows-95 grey. This is the single most common cause of a Win32 app
//!   looking wrong.
//! * **PerMonitorV2 DPI awareness** — without it the window is bitmap-scaled and
//!   blurry on any HiDPI or mixed-DPI setup.
//!
//! It is embedded via MSVC's own linker support (`/MANIFEST:EMBED` +
//! `/MANIFESTINPUT`) rather than through an `.rc` file and a resource-compiler
//! crate: the linker is already required for an MSVC build, so this needs no
//! build dependency and nothing to keep in version lockstep.

fn main() {
    // Re-run when the manifest changes; otherwise a manifest edit would sit
    // unused behind a cached build.
    println!("cargo:rerun-if-changed=res/freemkv.manifest");

    let target = std::env::var("TARGET").unwrap_or_default();
    if !target.contains("windows-msvc") {
        return;
    }

    // The linker resolves /MANIFESTINPUT relative to its own working directory,
    // which is not the crate root — pass an absolute path.
    let manifest = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("res")
        .join("freemkv.manifest");
    if !manifest.is_file() {
        // Never fail the build over a missing manifest: the app still runs, it
        // just looks unthemed. A hard error here would break `cargo publish`
        // and any packaging that trims non-source files.
        println!("cargo:warning=res/freemkv.manifest not found — controls will render unthemed");
        return;
    }

    // `-bins` (not the plain form) so the manifest lands on the executable and
    // is not passed when linking test harnesses or the lib target.
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}",
        manifest.display()
    );
}
