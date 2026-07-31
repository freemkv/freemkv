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
//!
//! The application **icon** is embedded the same dependency-free way. Without
//! it the executable has no icon at all and Windows substitutes the generic
//! default in the title bar, the taskbar, Alt-Tab and Explorer.
//!
//! An icon cannot go through `/MANIFESTINPUT`, but it does not need `rc.exe`
//! either: a `.res` file is a simple documented container of resource headers
//! plus payloads, and `link.exe` accepts `.res` inputs directly (it runs
//! `cvtres` internally). So `emit_icon_res` below *writes the `.res` itself*
//! from `res/freemkv.ico` — no resource compiler on the build machine, no
//! `winres`/`embed-resource` build dependency, and nothing extra required of
//! the CI runner. The `.ico` stays the single source of truth, and
//! `res/make-ico.py` regenerates it from the two SVGs.

/// Resource id given to the icon group. The Rust side loads it by this id (see
/// `IDI_APP` in `src/windows.rs`); Explorer independently uses the
/// lowest-numbered `RT_GROUP_ICON` as the executable's icon, so `1` serves
/// both purposes.
const IDI_APP: u16 = 1;

const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;

fn main() {
    // Re-run when the manifest changes; otherwise a manifest edit would sit
    // unused behind a cached build.
    println!("cargo:rerun-if-changed=res/freemkv.manifest");
    println!("cargo:rerun-if-changed=res/freemkv.ico");

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

    emit_icon_res();
}

/// Translate `res/freemkv.ico` into a `.res` file in `OUT_DIR` and hand it to
/// the linker.
///
/// A `.ico` file and an icon *resource* are deliberately not the same shape:
/// the file has one directory with byte offsets into itself, whereas the
/// executable stores each image as its own numbered `RT_ICON` and one
/// `RT_GROUP_ICON` directory that references those by resource id. Windows
/// then picks a frame per context — 16 px for the title bar, 32 px for
/// Alt-Tab, 256 px for Explorer's large view. Emitting only the file verbatim
/// would give Windows nothing it can select from.
fn emit_icon_res() {
    let root = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let ico_path = root.join("res").join("freemkv.ico");

    let ico = match std::fs::read(&ico_path) {
        Ok(b) => b,
        // Same rationale as the manifest: never fail the build over a missing
        // resource. The app runs fine, it just shows the default icon.
        Err(e) => {
            println!("cargo:warning=res/freemkv.ico unreadable ({e}) — app will have no icon");
            return;
        }
    };

    let images = match parse_ico(&ico) {
        Some(v) if !v.is_empty() => v,
        _ => {
            println!("cargo:warning=res/freemkv.ico is malformed — app will have no icon");
            return;
        }
    };

    // ── build the .res ──
    let mut res = Vec::new();
    // Every .res opens with a null entry (type 0, name 0, no payload) that
    // marks the file as 32-bit format; `cvtres` rejects the file without it.
    // Written literally rather than via `push_res_entry` because rc.exe emits
    // it fully zeroed — including the language field, which every *real* entry
    // sets to en-US.
    res.extend_from_slice(&0u32.to_le_bytes()); // data size
    res.extend_from_slice(&32u32.to_le_bytes()); // header size
    res.resize(32, 0); // type, name and the trailing fields: all zero

    // GRPICONDIR: the same 6-byte preamble as the file, then one 14-byte entry
    // per image — identical to the file's 16-byte entry except the trailing
    // 4-byte file offset becomes a 2-byte resource id.
    let mut group = Vec::new();
    group.extend_from_slice(&0u16.to_le_bytes()); // reserved
    group.extend_from_slice(&1u16.to_le_bytes()); // type: 1 = icon
    group.extend_from_slice(&(images.len() as u16).to_le_bytes());

    for (i, img) in images.iter().enumerate() {
        let id = IDI_APP + i as u16;
        group.push(img.width);
        group.push(img.height);
        group.push(img.colors);
        group.push(0); // reserved
        // Pillow writes wPlanes=0; `rc.exe` writes 1, and this field feeds
        // Windows' frame selection, so normalise rather than pass 0 through.
        group.extend_from_slice(&img.planes.max(1).to_le_bytes());
        group.extend_from_slice(&img.bit_count.to_le_bytes());
        group.extend_from_slice(&(img.data.len() as u32).to_le_bytes());
        group.extend_from_slice(&id.to_le_bytes());

        // MOVEABLE | DISCARDABLE — what rc.exe stamps on RT_ICON. Ignored by
        // modern Windows, but kept so the resource is byte-conventional.
        push_res_entry(&mut res, RT_ICON, id, 0x1010, img.data);
    }

    // MOVEABLE | PURE | DISCARDABLE, again matching rc.exe.
    push_res_entry(&mut res, RT_GROUP_ICON, IDI_APP, 0x1030, &group);

    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("freemkv-icon.res");
    if let Err(e) = std::fs::write(&out, &res) {
        println!(
            "cargo:warning=could not write {} ({e}) — no icon",
            out.display()
        );
        return;
    }

    // link.exe takes a .res as a plain input file and converts it in-process.
    // Absolute path: the linker's working directory is not the crate root.
    //
    // NOT `-bins`: that covers [[bin]] targets only, so the unit-test harness
    // linked without a resource section — and the window class registers
    // `class_icon: Icon::Id(IDI_APP)`, so building the real shell inside
    // `cargo test` failed in LoadIcon with 0x714, "The specified image file did
    // not contain a resource section", which winsafe turns into a panic. The
    // GUI test could not pass on any machine. (`-tests` is not the fix either:
    // it covers tests/*.rs integration targets, and this test lives in the lib.)
    // The unsuffixed form covers binaries, examples and every test harness, so
    // the test now exercises the real icon path rather than a stubbed one.
    println!("cargo:rustc-link-arg={}", out.display());
}

struct IcoImage<'a> {
    width: u8,
    height: u8,
    colors: u8,
    planes: u16,
    bit_count: u16,
    data: &'a [u8],
}

/// Split a `.ico` into its images. Returns `None` if the header does not look
/// like an icon file or any entry points outside the buffer.
///
/// Note `width`/`height` are single bytes and 256 is encoded as 0 — that quirk
/// is carried through to the group directory verbatim, which is what Windows
/// expects.
fn parse_ico(b: &[u8]) -> Option<Vec<IcoImage<'_>>> {
    let u16_at =
        |o: usize| -> Option<u16> { Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?)) };
    let u32_at =
        |o: usize| -> Option<u32> { Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?)) };

    if u16_at(0)? != 0 || u16_at(2)? != 1 {
        return None; // reserved must be 0, type must be 1 (icon)
    }
    let count = u16_at(4)? as usize;

    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let e = 6 + i * 16;
        let size = u32_at(e + 8)? as usize;
        let offset = u32_at(e + 12)? as usize;
        out.push(IcoImage {
            width: *b.get(e)?,
            height: *b.get(e + 1)?,
            colors: *b.get(e + 2)?,
            planes: u16_at(e + 4)?,
            bit_count: u16_at(e + 6)?,
            data: b.get(offset..offset.checked_add(size)?)?,
        });
    }
    Some(out)
}

/// Append one entry to a `.res` buffer.
///
/// Layout is fixed here because both the type and the name are numeric
/// ordinals (`0xFFFF` marker followed by the id), which makes the header a
/// constant 32 bytes and needs no name padding. Payloads are padded to a
/// 4-byte boundary, as each entry must start aligned.
fn push_res_entry(res: &mut Vec<u8>, type_id: u16, name_id: u16, memory_flags: u16, data: &[u8]) {
    const HEADER_SIZE: u32 = 32;

    res.extend_from_slice(&(data.len() as u32).to_le_bytes());
    res.extend_from_slice(&HEADER_SIZE.to_le_bytes());
    res.extend_from_slice(&0xFFFFu16.to_le_bytes()); // type is an ordinal…
    res.extend_from_slice(&type_id.to_le_bytes());
    res.extend_from_slice(&0xFFFFu16.to_le_bytes()); // …and so is the name
    res.extend_from_slice(&name_id.to_le_bytes());
    res.extend_from_slice(&0u32.to_le_bytes()); // data version
    res.extend_from_slice(&memory_flags.to_le_bytes());
    // 0x0409 = en-US. A neutral 0 also works, but rc.exe defaults to the
    // build locale and en-US keeps the output stable across machines.
    res.extend_from_slice(&0x0409u16.to_le_bytes());
    res.extend_from_slice(&0u32.to_le_bytes()); // version
    res.extend_from_slice(&0u32.to_le_bytes()); // characteristics

    res.extend_from_slice(data);
    res.resize(res.len().next_multiple_of(4), 0);
}
