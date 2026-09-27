//! Snap packaging drift: snap/snapcraft.yaml must build with the pinned toolchain, install files
//! that exist, declare the drive interfaces, and publish only when the store secret is set.

use std::path::Path;

fn read(rel: &str) -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

fn quoted_value(text: &str, key: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with(key))
        .unwrap_or_else(|| panic!("no `{key}` line"));
    line.split('"').nth(1).expect("value is quoted").to_string()
}

#[test]
fn snap_toolchain_is_the_msrv() {
    let snap = read("snap/snapcraft.yaml");
    let cargo = read("Cargo.toml");
    assert_eq!(
        quoted_value(&snap, "- RUST_TOOLCHAIN:"),
        quoted_value(&cargo, "rust-version =")
    );
}

#[test]
fn installed_files_exist() {
    let snap = read("snap/snapcraft.yaml");
    let mut checked = 0;
    for line in snap.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("install -Dm644 ") {
            let src = rest.split_whitespace().next().unwrap();
            assert!(
                Path::new(env!("CARGO_MANIFEST_DIR")).join(src).is_file(),
                "{src} is installed but missing"
            );
            checked += 1;
        }
    }
    assert_eq!(checked, 3, "desktop file, metainfo and icon");
    assert!(snap.contains("icon: res/freemkv-icon.svg"));
}

#[test]
fn both_apps_declare_the_drive_interfaces() {
    let snap = read("snap/snapcraft.yaml");
    assert!(snap.contains("  optical-write:\n    interface: optical-drive\n    write: true\n"));
    let plugs = [
        "optical-drive",
        "optical-write",
        "hardware-observe",
        "home",
        "removable-media",
        "network",
    ];
    let list: String = plugs.iter().map(|p| format!("      - {p}\n")).collect();
    assert_eq!(
        snap.matches(&format!("    plugs:\n{list}")).count(),
        2,
        "freemkv and freemkv-cli must both list {plugs:?}"
    );
    assert!(snap.contains("confinement: strict"));
}

#[test]
fn publishing_is_gated_on_the_store_secret() {
    let wf = read(".github/workflows/snap.yml");
    let publish = &wf[wf.find("\n  publish:").expect("publish job")..];
    assert!(publish.contains("needs.secrets.outputs.store == 'true'"));
    assert!(publish.contains("needs.smoke.result == 'success'"));
    assert!(publish.contains("needs.review.result == 'success'"));
    for channel in ["c=stable", "c=beta", "c=edge"] {
        assert!(publish.contains(channel), "{channel}");
    }
    assert!(
        publish.contains("for wf in ci.yml qa.yml"),
        "beta waits for green CI and qa"
    );
}

#[test]
fn app_may_own_its_bus_name() {
    let snap = read("snap/snapcraft.yaml");
    assert!(snap.contains(
        "  freemkv-dbus:\n    interface: dbus\n    bus: session\n    name: org.freemkv.FreeMKV\n"
    ));
    assert!(snap.contains("    slots: [freemkv-dbus]\n"));
}

#[test]
fn release_asset_does_not_wait_for_store_backed_checks() {
    let wf = read(".github/workflows/snap.yml");
    let job = &wf[wf.find("\n  release-asset:").expect("release-asset job")..];
    let needs = job
        .lines()
        .find(|l| l.trim_start().starts_with("needs:"))
        .unwrap();
    assert_eq!(needs.trim(), "needs: [build, smoke]");
}

#[test]
fn review_allows_exactly_the_two_store_grants() {
    let py = read("packaging/snap/store_checks.py");
    let start = py.find("KNOWN_GRANTS = (").expect("KNOWN_GRANTS");
    let body = &py[start..start + py[start..].find("\n)").unwrap()];
    let grants: Vec<&str> = body.split('"').skip(1).step_by(2).collect();
    assert_eq!(
        grants,
        [
            "declaration-snap-v2:plugs_connection:optical-write:optical-drive",
            "declaration-snap-v2:slots_connection:freemkv-dbus:dbus",
        ]
    );
    let wf = read(".github/workflows/snap.yml");
    assert!(wf.contains("store_checks.py review \"$status\" review.json"));
    assert!(wf.contains("unittest discover -s packaging/snap"));
}

#[test]
fn publish_runs_only_on_push_and_reports_honestly() {
    let wf = read(".github/workflows/snap.yml");
    let publish = &wf[wf.find("\n  publish:").expect("publish job")..];
    assert!(publish.contains("github.event_name == 'push'"));
    assert!(
        publish.contains("API error, retrying"),
        "a transient API error must retry"
    );
    assert!(publish.contains("store_checks.py upload \"$code\" \"$CHANNEL\" upload.log"));
    assert!(publish.contains("held for store manual review, NOT released"));
    assert!(publish.contains("held for store manual review (reason not reported)"));
    assert!(
        publish.contains(r#"[ "$ARM64_BUILD" != success ] && [ "$ARM64_BUILD" != skipped ]"#),
        "arm64 that failed or was cancelled must fail the job"
    );
    let arm = &publish[publish.find("ARM64_BUILD\" != skipped").unwrap()..];
    assert!(arm[..arm.find("fi\n").unwrap()].contains("rc=1"));
}

#[test]
fn gui_smoke_needs_the_bus_name_a_visible_window_and_a_live_app() {
    let wf = read(".github/workflows/snap.yml");
    assert!(wf.contains("NameHasOwner org.freemkv.FreeMKV"));
    assert!(
        wf.contains(r#"!/ 1x1\+/"#),
        "the 1x1 leader window must not count"
    );
    assert!(wf.contains("Map State: IsViewable"));
    assert!(wf.contains(r#"kill -0 "$app" 2>/dev/null || alive=false"#));
}
