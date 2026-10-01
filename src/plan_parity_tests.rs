//! Anti-drift §3: the same request through the CLI, app and server adapters builds the same
//! plan, so the engine makes the same calls and writes the same outputs; and one plan run
//! through the engine is goldened (bytes, verdict, codes).
use crate::engine::{KeyConfig, RipRequest, TitleStreams, gui_plan};
use crate::plan_core::*;
use freemkv_engine::Plan;

fn gui_req(source: &str, keys: KeyConfig, raw: bool, multipass: bool) -> RipRequest {
    RipRequest {
        source: source.into(),
        dest_dir: "/out".into(),
        titles: vec![],
        title_ids: Vec::new(),
        format: "Whole disc → ISO image".into(),
        audio_pids: vec![],
        sub_pids: vec![],
        title_pids: TitleStreams::Unspecified,
        explicit_streams: false,
        raw,
        force: false,
        filename_template: String::new(),
        decrypt_threads: 0,
        multipass,
        max_passes: if multipass { 3 } else { 0 },
        abort_lost_secs: 0,
        keep_iso: false,
        auto_eject: false,
        keys,
        seed: None,
        vid_from: None,
    }
}

fn gui_keys(local_only: bool, online_only: bool) -> KeyConfig {
    KeyConfig {
        keydb_path: "/k/keydb.cfg".into(),
        keyserver_url: if local_only {
            String::new()
        } else {
            "https://keys.example/keys".into()
        },
        keyserver_token: String::new(),
        online_only,
        local_only,
    }
}

fn cli(source: &str, dest: &str, flags: (bool, bool, bool), keys: CliKeys) -> Plan {
    plan(cli_request(
        source,
        dest,
        flags,
        cli_key_settings(&keys, "/k/keydb.cfg"),
    ))
}

#[cfg(feature = "server")]
fn server(device: &str, dest: &str, online: bool, max_retries: u8) -> Plan {
    let cfg = crate::server::config::Config {
        keydb_path: Some("/k/keydb.cfg".into()),
        key_source: if online { "online" } else { "local" }.into(),
        keyserver_url: "https://keys.example/keys".into(),
        max_retries,
        ..Default::default()
    };
    crate::server::ripper::server_plan(&cfg, device, dest)
}

// A live disc to a decrypted ISO from the local keydb: CLI `--keydb`, the app's "Local
// keydb only", the server's `key_source = local`.
#[test]
fn a_decrypted_disc_copy_is_one_plan_everywhere() {
    let dest = "iso:///out/Movie.iso";
    let c = cli(
        "disc:///dev/sr0",
        dest,
        (false, false, false),
        CliKeys {
            keydb: Some("/k/keydb.cfg".into()),
            ..Default::default()
        },
    );
    let g = gui_plan(
        &gui_req("disc:///dev/sr0", gui_keys(true, false), false, false),
        dest,
    );
    assert_eq!(c, g);
    #[cfg(feature = "server")]
    assert_eq!(c, server("/dev/sr0", dest, false, 0));
}

// Multipass recovery and an online-only key service.
#[test]
fn a_multipass_online_copy_is_one_plan_everywhere() {
    let dest = "iso:///out/Movie.iso";
    let c = cli(
        "disc:///dev/sr0",
        dest,
        (false, true, false),
        CliKeys {
            keydb: None,
            key_url: Some("https://keys.example/keys".into()),
            key_auth: None,
        },
    );
    // The CLI's keydb is its default path, here the app's and the server's setting.
    let g = gui_plan(
        &gui_req("disc:///dev/sr0", gui_keys(false, true), false, true),
        dest,
    );
    assert_eq!(c, g);
    #[cfg(feature = "server")]
    assert_eq!(c, server("/dev/sr0", dest, true, 3));
}

// A raw image copy: CLI `--raw` and the app's "Keep encrypted".
#[test]
fn a_raw_image_copy_is_one_plan_from_the_cli_and_the_app() {
    let dest = "iso:///out/Movie.iso";
    let c = cli(
        "iso:///in/Movie.iso",
        dest,
        (true, false, false),
        CliKeys {
            keydb: Some("/k/keydb.cfg".into()),
            ..Default::default()
        },
    );
    let g = gui_plan(
        &gui_req("/in/Movie.iso", gui_keys(true, false), true, false),
        dest,
    );
    assert_eq!(c, g);
}

// One plan, run: the engine's report for the CLI's and the app's image plan, goldened
// (the decrypted image's bytes, the copy verdict, and the no-key refusal code).
#[test]
fn parity_plan_image_copy() {
    use crate::ku_fixtures::*;
    let mut g =
        libfreemkv::test_util::Golden::new(env!("CARGO_MANIFEST_DIR"), "parity_plan_image_copy");
    let fx = bd_image(&[Some(K1)], 1);
    let dir = TempDir::new("plan-parity");
    let src = fx.write(dir.path(), "src.iso");
    for (tag, pool) in [("keyed", &[K1][..]), ("no-key", &[K2][..])] {
        let out = dir.path().join(format!("{tag}.iso"));
        let dest = format!("iso://{}", out.display());
        let c = cli(
            &format!("iso://{}", src.display()),
            &dest,
            (false, false, false),
            CliKeys {
                keydb: Some("/k/keydb.cfg".into()),
                ..Default::default()
            },
        );
        let a = gui_plan(
            &gui_req(
                &src.display().to_string(),
                gui_keys(true, false),
                false,
                false,
            ),
            &dest,
        );
        assert_eq!(c, a, "{tag}");
        let f = factory(&[(Answer::Keydb, pool)], &Calls::default());
        let with = freemkv_engine::RunWith {
            sources: Some(f),
            ..freemkv_engine::RunWith::default()
        };
        match freemkv_engine::run_with(&c, with, &freemkv_engine::NoopSink) {
            Ok(freemkv_engine::Report::Image { copy, .. }) => {
                g.kv(
                    &format!("{tag} copy"),
                    format_args!(
                        "good={} unreadable={} pending={} complete={}",
                        copy.bytes_good, copy.bytes_unreadable, copy.bytes_pending, copy.complete
                    ),
                );
                g.bytes(&format!("{tag} iso"), &std::fs::read(&out).unwrap());
            }
            Ok(other) => panic!("{tag}: {other:?}"),
            Err(e) => {
                g.kv(&format!("{tag} refused"), format_args!("E{}", e.code()));
            }
        }
    }
    g.check();
}
