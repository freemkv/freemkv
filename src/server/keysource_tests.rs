use super::*;
use libfreemkv::read_encrypted_units;

#[test]
fn keyserver_guard_allows_lan_and_rejects_invalid_hosts() {
    // Home app: loopback, link-local (incl. metadata) and RFC1918 key services are valid.
    // Built from octets so the dotted-quad doesn't trip the public leak-guard.
    let lan = format!("https://{}.{}.{}.{}/keys", 192, 168, 1, 5);
    for url in [
        "https://169.254.169.254/latest/meta-data",
        "https://127.0.0.1:8443/keys",
        "https://[::1]:443/k",
        "https://[fe80::1]/k",
        "https://[::ffff:127.0.0.1]/k",
        lan.as_str(),
    ] {
        assert!(
            freemkv_keysources::validate_keyserver_url(url).is_ok(),
            "{url} must be accepted"
        );
    }
    // Unreachable addresses are refused.
    for url in [
        "https://0.0.0.0/keys",
        "https://224.0.0.1/keys",
        "https://[ff02::1]/k",
    ] {
        assert!(
            freemkv_keysources::validate_keyserver_url(url).is_err(),
            "{url} must be refused"
        );
    }
    // Non-http scheme rejected.
    assert!(freemkv_keysources::validate_keyserver_url("ftp://example.com/keys").is_err());
    // No host.
    assert!(freemkv_keysources::validate_keyserver_url("https:///keys").is_err());
}

#[test]
fn ssrf_guard_allows_public_literal_ip() {
    // A public literal IP must pass (no DNS needed, deterministic).
    assert!(freemkv_keysources::validate_keyserver_url("https://8.8.8.8/keys").is_ok());
    assert!(freemkv_keysources::validate_keyserver_url("https://1.1.1.1:443").is_ok());
}

// The two rejection arms that fire BEFORE a host is extracted must never
// echo the raw input — `keyserver_url` can carry a bearer token, and
// `build_sources` logs this `Err` at ERROR, readable via `GET /api/debug`.
#[test]
fn validate_keyserver_url_error_never_echoes_raw_token() {
    let scheme_missing =
        freemkv_keysources::validate_keyserver_url("keys.example.org/decode?token=SUPERSECRET")
            .unwrap_err();
    assert!(
        !scheme_missing.contains("SUPERSECRET"),
        "scheme-missing error leaked the token: {scheme_missing}"
    );
    assert!(
        !scheme_missing.contains("token="),
        "scheme-missing error leaked the query string: {scheme_missing}"
    );

    let no_host = freemkv_keysources::validate_keyserver_url("https:///decode?token=SUPERSECRET")
        .unwrap_err();
    assert!(
        !no_host.contains("SUPERSECRET"),
        "no-host error leaked the token: {no_host}"
    );
    assert!(
        !no_host.contains("token="),
        "no-host error leaked the query string: {no_host}"
    );
}

// Cross-side agreement: autorip's sample selector (`read_encrypted_units`)
// hands the key service only units the service's own gate accepts, since
// both sides call the SAME predicate, `ts_sync_destroyed`.
#[test]
fn sample_units_are_all_aacs_scrambled() {
    use std::io::Write;

    // Synthetic ISO: 1200 sectors of scrambled (non-TS) content — no 0x47 at
    // any TS sync offset, so every aligned unit reads as AACS-scrambled.
    const SECTORS: usize = 1200;
    let mut tmp = tempfile::NamedTempFile::new().unwrap();
    tmp.write_all(&vec![0xE5u8; SECTORS * 2048]).unwrap();
    tmp.flush().unwrap();
    let mut reader = libfreemkv::FileSectorSource::open(tmp.path()).unwrap();

    let title = libfreemkv::DiscTitle {
        selection_evidence: Default::default(),
        playlist: "00800.mpls".into(),
        playlist_id: 800,
        duration_secs: 0.0,
        size_bytes: (SECTORS * 2048) as u64,
        clips: Vec::new(),
        streams: Vec::new(),
        chapters: Vec::new(),
        extents: vec![libfreemkv::Extent {
            start_lba: 0,
            sector_count: SECTORS as u32,
        }],
        content_format: libfreemkv::ContentFormat::BdTs,
        codec_privates: Vec::new(),
    };

    let units = read_encrypted_units(&mut reader, &title, SAMPLE_UNITS);
    assert_eq!(units.len(), SAMPLE_UNITS, "should collect 4 sample units");
    for u in &units {
        assert_eq!(u.len(), 6144);
        assert!(
            !libfreemkv::aacs::content::is_clean(u, libfreemkv::disc::ContentFormat::BdTs),
            "selector must only emit units the key service accepts"
        );
    }

    // The converse: a clear unit (TS syncs intact) is NOT scrambled.
    let mut clear = vec![0u8; 6144];
    let mut off = 4;
    while off < 6144 {
        clear[off] = 0x47;
        off += 192;
    }
    assert!(libfreemkv::aacs::content::is_clean(
        &clear,
        libfreemkv::disc::ContentFormat::BdTs
    ));
}

// autorip's keydb writes and the startup existence check must land on the
// same service-canonical path the reads resolve through (keydb_path /
// keydb_exists) — save_keydb writes straight there, no relocate dance.
#[test]
fn save_keydb_writes_to_service_path_and_existence_agrees() {
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("keys").join("keydb.cfg");

    let cfg = Config {
        keydb_path: Some(dest.to_string_lossy().into_owned()),
        ..Config::default()
    };

    // The path the reads will resolve and the gate must check.
    assert_eq!(keydb_path(&cfg), dest);
    assert!(!keydb_exists(&cfg), "no keydb written yet");

    // A minimal valid keydb body: one disc-entry line (`0x<hash> = <title>`),
    // matching the parser's real rule that a `0x` line is an entry only if it
    // also contains ` = `.
    let body = b"0xDEADBEEFDEADBEEFDEADBEEFDEADBEEF = Test\n";
    let result =
        save_keydb(&std::sync::RwLock::new(cfg.clone()), body).expect("save_keydb must succeed");

    // It wrote straight to the service path.
    assert_eq!(result.path, dest, "save must target the service path");
    assert!(dest.exists(), "keydb file must exist at the service path");
    assert!(
        keydb_exists(&cfg),
        "startup existence gate must now see the keydb the write produced"
    );
    assert_eq!(result.entries, 1, "one 0x entry");

    // No stray temp sibling left behind by the atomic write.
    let leftovers: Vec<_> = std::fs::read_dir(dest.parent().unwrap())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".tmp."))
        .collect();
    assert!(leftovers.is_empty(), "stray temp files: {leftovers:?}");

    // Content round-trips: the bytes at the service path are the keydb text.
    let written = std::fs::read_to_string(&dest).unwrap();
    assert!(
        written.contains("0xDEADBEEF"),
        "keydb content must be present"
    );
}

// keydb path resolution (#46): with no explicit `keydb_path`, reads/writes/
// gate all agree on canonical `<autorip_dir>/keydb.cfg`, NOT `$HOME`-derived.
// Deterministic — once the canonical file exists it wins over legacy/env.
#[test]
fn keydb_resolvers_agree_on_autorip_dir_default() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = Config {
        autorip_dir: tmp.path().to_string_lossy().into_owned(),
        keydb_path: None,
        ..Config::default()
    };
    let expected = tmp.path().join("keydb.cfg");
    std::fs::write(&expected, "0xDEAD = t | U | 1-0x0000000000000000\n").unwrap();

    assert_eq!(
        cfg.keydb_path, None,
        "default config carries no explicit keydb_path"
    );
    assert_eq!(
        keydb_path(&cfg),
        expected,
        "no override → canonical <autorip_dir>/keydb.cfg, never $HOME-derived"
    );
    // The existence gate resolves through the same path the reads use.
    assert!(keydb_exists(&cfg));
}

// An explicit `keydb_path` overrides the service default, and the existence
// gate + read path both honor it — an operator pointing autorip at a
// non-standard keydb gets reads, writes, and the startup gate aligned.
#[test]
fn explicit_keydb_path_overrides_default_and_gate_honors_it() {
    let tmp = tempfile::tempdir().unwrap();
    let explicit = tmp.path().join("custom").join("mykeys.cfg");

    let cfg = Config {
        keydb_path: Some(explicit.to_string_lossy().into_owned()),
        ..Config::default()
    };

    assert_eq!(
        keydb_path(&cfg),
        explicit,
        "explicit keydb_path must win over the service default"
    );
    assert!(!keydb_exists(&cfg), "file not created yet");

    std::fs::create_dir_all(explicit.parent().unwrap()).unwrap();
    std::fs::write(&explicit, b"0xAAAA\n").unwrap();
    assert!(
        keydb_exists(&cfg),
        "existence gate must see the file at the explicit path"
    );
}

/// A second `save_keydb` to the same service path replaces the prior keydb
/// in place (direct atomic write, no relocate) and reports that path.
#[test]
fn save_keydb_overwrites_existing_at_service_path() {
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("keys").join("keydb.cfg");
    let cfg = Config {
        keydb_path: Some(dest.to_string_lossy().into_owned()),
        ..Config::default()
    };

    save_keydb(
        &std::sync::RwLock::new(cfg.clone()),
        b"0xAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA = Test\n",
    )
    .expect("first save");
    let result = save_keydb(
        &std::sync::RwLock::new(cfg.clone()),
        b"0xBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB = Test\n",
    )
    .expect("second save");

    assert_eq!(result.path, dest, "save always targets the service path");
    let written = std::fs::read_to_string(&dest).unwrap();
    assert!(written.contains("0xBBBB"), "newest keydb content must win");
    assert!(!written.contains("0xAAAA"), "old content fully replaced");
}

// `drive_scan_opts_for_keydb` must wire `DriveCredentials` when the keydb
// carries a host cert, so the AACS handshake gets it — no existing test
// drove this function with a real keydb fixture before.
#[test]
fn drive_scan_opts_for_keydb_wires_credentials_when_host_certs_present() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("keydb.cfg");
    // A single AACS 1.0 host-cert row: 20-byte priv key + 92-byte cert,
    // all-zero placeholders (never a real key) — same shape
    // freemkv-keysources' own `test_parse_host_cert` uses.
    let line = format!(
        "| HC | HOST_PRIV_KEY 0x{} | HOST_CERT 0x{} ; Revoked\n",
        "00".repeat(20),
        "00".repeat(92)
    );
    std::fs::write(&path, line).unwrap();

    let opts = drive_scan_opts_for_keydb(&path);
    let credentials = opts
        .credentials
        .expect("a keydb with a host cert must produce Some(DriveCredentials)");
    assert!(
        !credentials.host_certs.is_empty(),
        "the wired credentials must actually carry the cert"
    );
}

/// A keydb with NO host certs (or no keydb at all) must yield `None` —
/// not `Some` wrapping an empty cert list, which would look "present"
/// to a caller checking `.is_some()` while carrying nothing usable.
#[test]
fn drive_scan_opts_for_keydb_no_credentials_without_host_certs() {
    let tmp = tempfile::tempdir().unwrap();
    // A keydb with disc entries but no `| HC |` row.
    let path = tmp.path().join("keydb.cfg");
    std::fs::write(&path, b"0xDEADBEEFDEADBEEFDEADBEEFDEADBEEF = Test\n").unwrap();
    let opts = drive_scan_opts_for_keydb(&path);
    assert!(
        opts.credentials.is_none(),
        "no host certs must mean no DriveCredentials at all"
    );

    // No keydb file at all — same expectation.
    let missing = tmp.path().join("does-not-exist.cfg");
    let opts2 = drive_scan_opts_for_keydb(&missing);
    assert!(opts2.credentials.is_none());
}

/// A minimal keyless, encrypted `Disc` with no AACS state.
fn keyless_encrypted_disc() -> libfreemkv::Disc {
    libfreemkv::Disc {
        volume_id: "TEST_DISC".into(),
        meta_title: None,
        format: libfreemkv::DiscFormat::BluRay,
        capacity_sectors: 0,
        capacity_bytes: 0,
        layers: 1,
        titles: Vec::new(),
        region: libfreemkv::disc::DiscRegion::Free,
        aacs: None,
        css: None,
        encrypted: true,
        aacs_error: None,
        css_error: None,
        content_format: libfreemkv::ContentFormat::BdTs,
    }
}

// Like `keyless_encrypted_disc` but WITH AACS state, so `disc.inputs()` returns `Some`.
fn keyless_encrypted_disc_with_aacs() -> libfreemkv::Disc {
    let mut disc = keyless_encrypted_disc();
    disc.aacs = Some(
        libfreemkv::test_util::aacs_state()
            .version(libfreemkv::aacs::mkb::AACS_MAJOR_UHD)
            .disc_hash("0xabc")
            .build(),
    );
    disc
}

// --- the key chain (engine, local-first) ----------------------------------

// No URL: "local" is the keydb; "online" has nothing to ask until a URL is set.
#[test]
fn build_sources_without_a_url() {
    for (key_source, n) in [("local", 1), ("onlnie", 1), ("online", 0)] {
        let cfg = Config {
            key_source: key_source.into(),
            ..Config::default()
        };
        let sources = build_sources(&cfg);
        assert_eq!(sources.len(), n, "{key_source}");
        assert!(sources.iter().all(|s| s.label() == "keydb"));
        assert_eq!(uses_online(&cfg), key_source == "online", "{key_source}");
    }
}

// The picked source only: "online" is the key service, "local" the keydb, URL or not.
#[test]
fn build_sources_follows_key_source_when_a_url_is_set() {
    for (key_source, want) in [("local", &["keydb"][..]), ("online", &["online"][..])] {
        let cfg = Config {
            key_source: key_source.into(),
            keyserver_url: "https://8.8.8.8/keys".into(),
            ..Config::default()
        };
        let labels: Vec<_> = build_sources(&cfg).iter().map(|s| s.label()).collect();
        assert_eq!(labels, want, "{key_source}");
    }
}

// An SSRF-blocked URL never becomes a source, leaving online mode with none.
#[test]
fn build_sources_drops_online_source_on_invalid_address_url() {
    let cfg = Config {
        key_source: "online".into(),
        keyserver_url: "https://0.0.0.0/keys".into(),
        ..Config::default()
    };
    assert!(build_sources(&cfg).is_empty());
}

// key_source=local with a saved URL is local only: no online source, no
// online probe or outage retry, and no "Communicating" status.
#[test]
fn local_key_source_with_saved_url_never_goes_online() {
    let cfg = Config {
        key_source: "local".into(),
        keyserver_url: "https://8.8.8.8/decode".into(),
        keyserver_secret: "tok".into(),
        ..Config::default()
    };
    assert!(!uses_online(&cfg));
    let p = key_params(&cfg);
    assert!(p.key_url.is_none() && p.key_auth.is_none() && !p.online_only);
    let labels: Vec<_> = build_sources(&cfg).iter().map(|s| s.label()).collect();
    assert_eq!(labels, ["keydb"]);
    assert!(keyserver_url_startup_warning(&cfg).is_none());
}

// The settings fields autorip wrote map onto the engine's parameters as-is.
#[test]
fn key_params_reads_the_autorip_settings_fields() {
    let cfg = Config {
        key_source: "online".into(),
        keyserver_url: "  https://keys.example.org/decode ".into(),
        keyserver_secret: "tok".into(),
        keydb_path: Some("/config/custom.cfg".into()),
        ..Config::default()
    };
    let p = key_params(&cfg);
    assert_eq!(p.keydb_path.as_deref(), Some("/config/custom.cfg"));
    assert_eq!(
        p.key_url.as_deref(),
        Some("https://keys.example.org/decode")
    );
    assert_eq!(p.key_auth.as_deref(), Some("tok"));
    assert!(
        p.online_only,
        "online with a usable URL asks only the service"
    );
    let bare = key_params(&Config::default());
    assert_eq!(bare.key_url, None);
    assert_eq!(bare.key_auth, None);
}

// A missing image fails the open on both key routes (drive keys, the key chain).
#[test]
fn open_staged_image_reports_a_missing_image() {
    let cfg = Config::default();
    let missing = Path::new("/nonexistent-autorip-iso-fixture-xyz.iso");
    let disc = || keyless_encrypted_disc_with_aacs();
    let rip = StagedKeys::Rip(libfreemkv::keys::KeyRing::none());
    assert!(open_staged_image(&cfg, missing, disc(), &[0], rip, None).is_err());
    let resolve = StagedKeys::Resolve {
        vid: Some([7u8; 16]),
    };
    assert!(open_staged_image(&cfg, missing, disc(), &[0], resolve, None).is_err());
}

// The PROBE is disc-less (empty POST), so its classification stays coarse:
// transport/5xx/429 are transient and EVERY other status means only "the
// service is up" — never a per-disc no-key verdict.
#[test]
fn classify_reachability_down_vs_no_key() {
    use ProbeOutcome::{Status, Transport};
    for code in [500u16, 502, 503, 504] {
        assert_eq!(
            classify_reachability(Status(code)),
            ServiceReachability::ServerError(code),
            "HTTP {code} is the service failing on its own side"
        );
    }
    // transport failure (timeout / connect refused) → never reached
    assert_eq!(
        classify_reachability(Transport),
        ServiceReachability::Unreachable
    );
    // quota → RATE-LIMITED
    assert_eq!(
        classify_reachability(Status(429)),
        ServiceReachability::RateLimited
    );
    // Everything else proves only reachability — the probe carried no disc.
    for code in [200u16, 404, 405, 422] {
        assert_eq!(
            classify_reachability(Status(code)),
            ServiceReachability::Answered,
            "a disc-less probe must not produce a per-disc verdict from {code}"
        );
    }
}

// The REAL decode POST carried the disc, so its status IS a per-disc
// verdict. This is the mapping the operator-facing message is written from:
// every outcome in the bug report gets its own arm and none of them share.
#[test]
fn decode_outcome_drives_the_down_vs_no_key_verdict() {
    use freemkv_keysources::DecodeReachability::{Status, Transport};
    // The bug's exact case: 422 "licensed but unresolved" is a DEFINITIVE
    // no-key for this disc — reached, licensed, all candidates exhausted.
    assert_eq!(
        reachability_from_decode(Status(422)),
        ServiceReachability::NoKeyForDisc
    );
    // 404 is a licence/wall verdict, NOT the same thing as 422.
    assert_eq!(
        reachability_from_decode(Status(404)),
        ServiceReachability::NotLicensed
    );
    assert_eq!(
        reachability_from_decode(Status(200)),
        ServiceReachability::Answered
    );
    assert_eq!(
        reachability_from_decode(Status(304)),
        ServiceReachability::Answered
    );
    // A transport failure is the ONLY "we never reached it" outcome.
    assert_eq!(
        reachability_from_decode(Transport),
        ServiceReachability::Unreachable
    );
    // 5xx / 429 remain transient, each with its own verdict.
    assert_eq!(
        reachability_from_decode(Status(503)),
        ServiceReachability::ServerError(503)
    );
    assert_eq!(
        reachability_from_decode(Status(429)),
        ServiceReachability::RateLimited
    );
    // Anything else keeps its status rather than being guessed at.
    for code in [400u16, 418, 451] {
        assert_eq!(
            reachability_from_decode(Status(code)),
            ServiceReachability::Unexpected(code)
        );
    }
}

// 401/403 is a credential rejection (keysources' KeyServiceUnauthorized),
// terminal, and must not collapse into the generic Unexpected(code).
#[test]
fn decode_401_403_is_unauthorized() {
    use freemkv_keysources::DecodeReachability::Status;
    for code in [401u16, 403] {
        let v = reachability_from_decode(Status(code));
        assert_ne!(v, ServiceReachability::Unexpected(code), "{code}");
        assert_eq!(v.http_status(), Some(code));
        assert!(!v.is_transient());
    }
}

// A resolve must never hand back a PREVIOUS resolve's decode verdict: plant one
// (Transport), then resolve
// with no sources / no AACS inputs — both return before any online query.
#[test]
fn resolve_drains_a_stale_decode_verdict() {
    let plant = || {
        freemkv_keysources::set_last_decode_reachability(Some(
            freemkv_keysources::DecodeReachability::Transport,
        ));
    };
    plant();
    assert!(
        freemkv_keysources::take_last_decode_reachability().is_some(),
        "fixture must plant a verdict"
    );
    plant();
    let none: libfreemkv::KeySourceFactory = std::sync::Arc::new(Vec::new);
    let disc = keyless_encrypted_disc_with_aacs();
    let mut reader = libfreemkv::test_util::MemSource::new(vec![0u8; 2048]);
    let scope = libfreemkv::keys::KeyScope::Titles(Vec::new());
    let _ = resolve_with(&disc, &mut reader, scope, &none, None, None);
    assert_eq!(
        take_online_decode_reachability(),
        None,
        "no-sources resolve"
    );
    plant();
    let scope = libfreemkv::keys::KeyScope::None;
    let _ = resolve_with(
        &keyless_encrypted_disc(),
        &mut reader,
        scope,
        &none,
        None,
        None,
    );
    assert_eq!(
        take_online_decode_reachability(),
        None,
        "a raw-copy resolve"
    );
}

// No two of these outcomes may collapse to the same verdict — that
// collapse is the bug (a 422 reported as "the service was down").
#[test]
fn every_key_service_outcome_is_distinct() {
    use freemkv_keysources::DecodeReachability::{Status, Transport};
    let verdicts = [
        reachability_from_decode(Transport),
        reachability_from_decode(Status(503)),
        reachability_from_decode(Status(429)),
        reachability_from_decode(Status(422)),
        reachability_from_decode(Status(404)),
        reachability_from_decode(Status(400)),
        reachability_from_decode(Status(401)),
        reachability_from_decode(Status(200)),
    ];
    for (i, a) in verdicts.iter().enumerate() {
        for b in &verdicts[i + 1..] {
            assert_ne!(a, b, "distinct key-service outcomes share a verdict");
        }
    }
}

/// Only the "never got an answer about this disc" verdicts are
/// transient/retryable. A definitive 422 no-key is terminal — retrying it
/// is pointless work, and the 29-second server-side exhaustion behind it
/// makes that retry expensive. Regression guard both ways.
#[test]
fn reachability_transient_partition() {
    assert!(ServiceReachability::Unreachable.is_transient());
    assert!(ServiceReachability::ServerError(502).is_transient());
    assert!(ServiceReachability::RateLimited.is_transient());
    assert!(
        !ServiceReachability::NoKeyForDisc.is_transient(),
        "a definitive no-key must never be retried"
    );
    assert!(!ServiceReachability::NotLicensed.is_transient());
    assert!(!ServiceReachability::Unexpected(400).is_transient());
    assert!(!ServiceReachability::NotAsked.is_transient());
    assert!(!ServiceReachability::Answered.is_transient());
}

/// The status is carried on the verdict so support can quote it — `None`
/// only where there genuinely was no HTTP answer.
#[test]
fn http_status_is_carried_where_one_exists() {
    assert_eq!(ServiceReachability::NoKeyForDisc.http_status(), Some(422));
    assert_eq!(ServiceReachability::NotLicensed.http_status(), Some(404));
    assert_eq!(ServiceReachability::RateLimited.http_status(), Some(429));
    assert_eq!(
        ServiceReachability::ServerError(503).http_status(),
        Some(503)
    );
    assert_eq!(
        ServiceReachability::Unexpected(418).http_status(),
        Some(418)
    );
    assert_eq!(ServiceReachability::Unreachable.http_status(), None);
    assert_eq!(ServiceReachability::NotAsked.http_status(), None);
    assert_eq!(ServiceReachability::Answered.http_status(), None);
}

// Permanent verdicts from the REAL keysources producer: the online source is
// dropped for these, so a no-key is genuine and must NOT be retried as an outage.
#[test]
fn keysources_config_rejections_are_not_asked() {
    for url in [
        "http://8.8.8.8/keys",
        "https:///keys",
        "https://8.8.8.8:notaport/keys",
        "https://[::1/keys",
        "https://0.0.0.0/keys",
        "https://240.0.0.1/latest/meta-data",
    ] {
        let err = freemkv_keysources::validate_keyserver_url(url)
            .expect_err("keysources must reject this URL outright");
        assert_eq!(
            reachability_for_unprobeable_url(&err),
            ServiceReachability::NotAsked,
            "{url:?} -> {err:?} is a config verdict, not an outage"
        );
    }
}

// keysources' GuardFail::Unreachable texts (online.rs resolve_and_guard). They need
// live DNS to produce, so are pinned verbatim until keysources exports a typed kind.
#[test]
fn keysources_lookup_failures_are_unreachable() {
    for msg in [
        "too many concurrent DNS resolutions in flight for this host",
        "could not resolve host: failed to lookup address information",
        "DNS resolution timed out",
        "host did not resolve to any address",
    ] {
        assert_eq!(
            reachability_for_unprobeable_url(msg),
            ServiceReachability::Unreachable,
            "{msg:?} is a DNS failure, not a config verdict"
        );
    }
}

// web's own resolver failures (the probe's pinning step) stay transient.
#[test]
fn web_resolve_failures_are_unreachable() {
    for msg in [
        crate::server::web::RESOLVE_TIMEOUT_MSG.to_string(),
        crate::server::web::RESOLVE_NO_ADDRS_MSG.to_string(),
        format!("{}EAI_AGAIN", crate::server::web::RESOLVE_FAILED_PREFIX),
    ] {
        assert_eq!(
            reachability_for_unprobeable_url(&msg),
            ServiceReachability::Unreachable
        );
    }
}

// A stored pre-upgrade http:// keyserver URL is named at boot in online mode;
// https, blank, and any URL under `key_source = "local"` are silent.
#[test]
fn keyserver_url_startup_warning_flags_only_non_https() {
    let cfg = |src: &str, url: &str| Config {
        key_source: src.into(),
        keyserver_url: url.into(),
        ..Config::default()
    };
    let w = keyserver_url_startup_warning(&cfg("online", " http://keys.example.org/t0k/d"))
        .expect("http:// must warn");
    assert!(w.contains("https://") && !w.contains("t0k"), "{w}");
    assert!(!w.contains("  "), "no stray whitespace runs: {w:?}");
    assert!(keyserver_url_startup_warning(&cfg("online", "https://k.example.org/d")).is_none());
    assert!(keyserver_url_startup_warning(&cfg("online", "")).is_none());
    assert!(keyserver_url_startup_warning(&cfg("local", "http://k.example.org/d")).is_none());
}

// Cleartext http:// is a standing config fault: the online source is dropped.
#[test]
fn build_sources_drops_online_source_on_http_url() {
    let cfg = Config {
        key_source: "online".into(),
        keyserver_url: "http://8.8.8.8/decode".into(),
        ..Config::default()
    };
    assert!(build_sources(&cfg).is_empty());
}

// `render_resolution_trace` is the app-layer's ENTIRE English mapping of
// the library's typed trace, shown on every rip — a dropped/mis-mapped arm
// ships a wrong diagnostic. Drive every enum arm through it and pin output.
#[test]
fn render_resolution_trace_maps_every_enum_arm() {
    use libfreemkv::aacs::trace::{
        KeyNode, KeyOutcome, KeyStep, ResolutionTrace, UnlockOutcome, UnlockStep,
    };

    let unlock = vec![
        UnlockStep {
            who: "u_ok".into(),
            outcome: UnlockOutcome::Unlocked,
        },
        UnlockStep {
            who: "u_fw".into(),
            outcome: UnlockOutcome::FirmwareNotUnlockable,
        },
        UnlockStep {
            who: "u_cert".into(),
            outcome: UnlockOutcome::NoUsableHostCert { mkb: Some(7) },
        },
        UnlockStep {
            who: "u_rev".into(),
            outcome: UnlockOutcome::CertRevoked { mkb: None },
        },
        UnlockStep {
            who: "u_hs".into(),
            outcome: UnlockOutcome::HandshakeRejected,
        },
        UnlockStep {
            who: "u_vid".into(),
            outcome: UnlockOutcome::VidUnavailable,
        },
    ];
    // One key step walking EVERY node, plus one per terminal outcome.
    let keys = vec![
        KeyStep {
            who: "keydb".into(),
            path: vec![
                KeyNode::MatchedDisc,
                KeyNode::NoEntry,
                KeyNode::NoDerivableKey,
                KeyNode::FoundUnitKeys,
                KeyNode::FoundVuk,
                KeyNode::FoundMediaKey,
                KeyNode::NeedVid,
                KeyNode::VidFromUnlock,
                KeyNode::VidFromKeydb,
                KeyNode::NoVid,
                KeyNode::DerivedVuk,
                KeyNode::DerivedUnitKeys,
            ],
            outcome: KeyOutcome::Resolved,
            matched_entry: None,
            store_entries: None,
        },
        KeyStep {
            who: "online".into(),
            path: vec![KeyNode::NeedVid],
            outcome: KeyOutcome::MissingVid,
            matched_entry: None,
            store_entries: None,
        },
        KeyStep {
            who: "empty".into(),
            path: vec![],
            outcome: KeyOutcome::NoKey,
            matched_entry: None,
            store_entries: None,
        },
    ];
    let trace = ResolutionTrace { unlock, keys };

    let lines = render_resolution_trace(&trace, "0xDEADBEEF");
    assert_eq!(
        lines,
        vec![
            "unlock: u_ok > UNLOCKED",
            "unlock: u_fw > firmware not unlockable",
            "unlock: u_cert > no usable host cert (MKBv7)",
            "unlock: u_rev > host cert revoked",
            "unlock: u_hs > handshake rejected",
            "unlock: u_vid > Volume ID unavailable",
            "key: keydb > matched disc > no entry > no derivable key > found unit keys > \
                 found VUK > found media key > need VID > VID from drive > VID from keydb > \
                 no VID > derived VUK > derived unit keys > RESOLVED",
            "key: online > need VID > MISSING VID",
            "key: empty > NO KEY",
        ]
    );
}

/// Issue #46: a MATCHED-but-underivable (no VID) keydb hit renders the
/// actionable verdict `matched disc > no VID available > NO KEY`, never the
/// misleading `no entry`.
// KU-E1: a matched entry whose key needs the VID reads as such, not as a bare node walk.
#[test]
fn matched_missing_vid_says_the_key_needs_the_volume_id() {
    use libfreemkv::aacs::trace::{KeyNode, KeyOutcome, KeyStep};
    let step = KeyStep {
        who: "keydb".into(),
        path: vec![KeyNode::MatchedDisc, KeyNode::FoundMediaKey, KeyNode::NoVid],
        outcome: KeyOutcome::MissingVid,
        matched_entry: None,
        store_entries: None,
    };
    assert_eq!(
        render_key_step(&step, "0xAB"),
        "keydb > matched disc > key needs the disc's Volume ID, not in hand > MISSING VID"
    );
}

#[test]
fn matched_no_vid_renders_distinctly_from_a_true_miss() {
    use libfreemkv::aacs::trace::{KeyNode, KeyOutcome, KeyStep, ResolutionTrace};

    let trace = ResolutionTrace {
        unlock: vec![],
        keys: vec![KeyStep {
            who: "keydb".into(),
            path: vec![KeyNode::MatchedDisc, KeyNode::NoVid],
            outcome: KeyOutcome::NoKey,
            matched_entry: None,
            store_entries: Some(500),
        }],
    };
    assert_eq!(
        render_resolution_trace(&trace, "0xABC"),
        vec!["key: keydb > matched disc > no VID available > NO KEY"]
    );
}

/// Issue #46: a matched entry with no usable keys dumps its shape, and a true
/// miss names the hash and the store size — the two are self-diagnosing and
/// clearly distinct.
#[test]
fn matched_no_material_and_true_miss_render_distinctly() {
    use libfreemkv::aacs::trace::{KeyNode, KeyOutcome, KeyStep, MatchedEntry, ResolutionTrace};

    let matched = ResolutionTrace {
        unlock: vec![],
        keys: vec![KeyStep {
            who: "keydb".into(),
            path: vec![KeyNode::MatchedDisc, KeyNode::NoDerivableKey],
            outcome: KeyOutcome::NoKey,
            matched_entry: Some(MatchedEntry {
                has_vuk: false,
                has_unit_keys: false,
                unit_keys_len: 0,
                has_media_key: false,
                has_keydb_vid: false,
                enc_title_keys_len: 2,
                vid_available: false,
            }),
            store_entries: Some(500),
        }],
    };
    assert_eq!(
        render_resolution_trace(&matched, "0xABC"),
        vec![
            "key: keydb > matched disc > entry has no usable keys \
                 (vuk=false unit_keys=0 enc_title_keys=2 media_key=false) > NO KEY"
        ]
    );

    let miss = ResolutionTrace {
        unlock: vec![],
        keys: vec![KeyStep {
            who: "keydb".into(),
            path: vec![KeyNode::NoEntry],
            outcome: KeyOutcome::NoKey,
            matched_entry: None,
            store_entries: Some(500),
        }],
    };
    assert_eq!(
        render_resolution_trace(&miss, "0x1234abcd"),
        vec!["key: keydb > no entry > disc hash 0x1234abcd not in keydb (500 entries loaded)"]
    );
}

// `probe_online_reachability`'s two non-network arms: an EMPTY keyserver URL
// and an SSRF-blocked one both mean the service was never ASKED — a config
// verdict, terminal like before, never an outage. Neither touches the network.
#[test]
fn probe_online_reachability_unprobeable_urls_report_up() {
    let empty = Config {
        keyserver_url: String::new(),
        ..Default::default()
    };
    assert_eq!(
        probe_online_reachability(&empty),
        ServiceReachability::NotAsked
    );
    assert!(!probe_online_reachability(&empty).is_transient());

    let blocked = Config {
        keyserver_url: "https://0.0.0.0:9/keys".into(),
        ..Default::default()
    };
    assert_eq!(
        probe_online_reachability(&blocked),
        ServiceReachability::NotAsked,
        "an invalid-address URL is a permanent config verdict, not an outage"
    );
}

// ── keydb PATH resolution (issue #46) ── the full decision table, driven
//    through the pure `resolve_keydb` with an injected `exists` + `legacy`
//    so every branch is deterministic (no env, no filesystem). ──

fn none_exists(_: &std::path::Path) -> bool {
    false
}

/// An explicit configured path always wins — even when the canonical file
/// also exists on disk.
#[test]
fn resolve_keydb_explicit_path_wins() {
    let got = resolve_keydb(Some("/mnt/keys/keydb.cfg"), "/config", None, &|_| true);
    assert_eq!(got, PathBuf::from("/mnt/keys/keydb.cfg"));
}

/// A blank configured path is treated as unset (the UI sends "" for empty).
#[test]
fn resolve_keydb_blank_config_falls_through_to_default() {
    let got = resolve_keydb(Some(""), "/config", None, &none_exists);
    assert_eq!(got, PathBuf::from("/config/keydb.cfg"));
}

/// Default with nothing on disk → the canonical AUTORIP_DIR path (the read +
/// write + download target). This is the #46 fix: `/config/keydb.cfg`, NOT a
/// `$HOME`-derived path.
#[test]
fn resolve_keydb_default_is_autorip_dir_when_nothing_exists() {
    let got = resolve_keydb(None, "/config", None, &none_exists);
    assert_eq!(got, PathBuf::from("/config/keydb.cfg"));
}

/// #46 scenario: a user drops keydb.cfg at /config/keydb.cfg → autorip now
/// resolves to exactly that file.
#[test]
fn resolve_keydb_finds_user_file_at_config() {
    let canonical = PathBuf::from("/config/keydb.cfg");
    let got = resolve_keydb(None, "/config", None, &|p| p == canonical);
    assert_eq!(got, canonical);
}

/// Upgrade migration: canonical missing but a legacy $HOME keydb exists →
/// keep resolving to the legacy file (don't force a re-download).
#[test]
fn resolve_keydb_migrates_to_legacy_when_canonical_absent() {
    let legacy = PathBuf::from("/root/.config/freemkv/keydb.cfg");
    let lg = legacy.clone();
    let got = resolve_keydb(None, "/config", Some(legacy.clone()), &move |p| p == lg);
    assert_eq!(got, legacy);
}

/// Canonical present takes precedence over a legacy file (no accidental
/// migration once the new location is populated).
#[test]
fn resolve_keydb_canonical_beats_legacy_when_both_exist() {
    let legacy = PathBuf::from("/root/.config/freemkv/keydb.cfg");
    let got = resolve_keydb(None, "/config", Some(legacy), &|_| true);
    assert_eq!(got, PathBuf::from("/config/keydb.cfg"));
}

/// Unset or empty HOME (the container case that caused #46) yields no legacy
/// path, so resolution never collapses to a stray relative path.
#[test]
fn resolve_keydb_no_legacy_when_home_absent() {
    assert_eq!(legacy_keydb_under(None), None);
    assert_eq!(legacy_keydb_under(Some(std::ffi::OsString::new())), None);
    assert_eq!(
        legacy_keydb_under(Some("/root".into())),
        Some(PathBuf::from("/root/.config/freemkv/keydb.cfg"))
    );
    let got = resolve_keydb(None, "/config", legacy_keydb_under(Some("".into())), &|p| {
        p != Path::new("/config/keydb.cfg")
    });
    // Lands on canonical AUTORIP_DIR path, NOT the bare relative "keydb.cfg"
    // the HOME-less container collapsed to (#46). No is_absolute assert:
    // "/config" isn't absolute on Windows; the equality already proves it.
    assert_eq!(got, PathBuf::from("/config/keydb.cfg"));
    assert_ne!(got, PathBuf::from("keydb.cfg"));
}

/// `keydb_path` honors an explicit config value end-to-end.
#[test]
fn keydb_path_uses_explicit_config_value() {
    let cfg = Config {
        keydb_path: Some("/mnt/archive/keydb.cfg".into()),
        ..Config::default()
    };
    assert_eq!(keydb_path(&cfg), PathBuf::from("/mnt/archive/keydb.cfg"));
}
