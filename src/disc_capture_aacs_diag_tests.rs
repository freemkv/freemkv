use super::*;
use libfreemkv::disc::DiscRegion;
use libfreemkv::{AacsState, Disc, DiscFormat};

// Distinctive secrets. The unit key is the rip's (it encrypts the fixture image),
// never the disc's; the VID and MKB bytes sit in the scanned `AacsState`.
const SECRET_UNIT_KEY: [u8; 16] = *b"\xC1unit-key-KU-P1!";
const SECRET_VID: [u8; 16] = [0xEF; 16];
const SECRET_MKB: [u8; 4] = [0x78, 0x9a, 0xbc, 0xde];

fn disc_with(aacs: Option<AacsState>) -> Disc {
    Disc {
        volume_id: "TEST_DISC".to_string(),
        meta_title: None,
        format: DiscFormat::Uhd,
        capacity_sectors: 0,
        capacity_bytes: 0,
        layers: 1,
        titles: Vec::new(),
        region: DiscRegion::Free,
        aacs,
        css: None,
        encrypted: true,
        aacs_error: None,
        css_error: None,
        content_format: libfreemkv::ContentFormat::BdTs,
    }
}

// A scanned AACS state: a real disc hash plus the known VID, MKB and
// `Unit_Key_RO.inf` bytes, so artifacts can leak the hash but none of those.
fn aacs_with_secrets(disc_hash: &str) -> AacsState {
    libfreemkv::test_util::aacs_state()
        .version(2)
        .bus_encryption(true)
        .mkb_version(Some(77))
        .disc_hash(disc_hash)
        .volume_id(SECRET_VID)
        .uk_ro(vec![0x12, 0x34, 0x56])
        .mkb(SECRET_MKB.to_vec())
        .build()
}

#[test]
fn aacs_present_puts_disc_hash_into_profile() {
    // KEYDB.cfg rows are keyed `0x<40hex>`; the profile records that exact form.
    let raw = "0xaabbccddeeff00112233445566778899aabbccdd";
    let bare = "0xaabbccddeeff00112233445566778899aabbccdd";
    let disc = disc_with(Some(aacs_with_secrets(raw)));

    let json = aacs_json(&disc);
    assert!(json.contains("\"aacs_captured\": true"), "{json}");
    assert!(
        json.contains(&format!("\"disc_hash\": \"{bare}\"")),
        "disc hash must be the bare keydb lookup key: {json}"
    );
    assert!(json.contains("\"aacs_generation\": 2"), "{json}");
    assert!(json.contains("\"mkb_version\": 77"), "{json}");
    assert!(json.contains("\"bus_encryption\": true"), "{json}");
    // volume_id is non-zero, so a VID was available — but only the boolean.
    assert!(json.contains("\"vid_available\": true"), "{json}");

    let body = issue_body(
        &disc,
        &DiscSummary {
            file_count: 3,
            total_bytes: 100,
            skipped: Vec::new(),
        },
        "QUJD",
    )
    .0;
    assert!(body.contains("## AACS diagnostics"), "{body}");
    assert!(
        body.contains(bare),
        "issue body must name the disc hash: {body}"
    );
}

#[test]
fn a_disc_with_no_aacs_state_records_no_crypto_shape() {
    let disc = disc_with(None);
    let json = aacs_json(&disc);
    assert!(json.contains("\"aacs_captured\": false"), "{json}");
    // No crypto-shape fields when nothing was captured (assert the JSON key form only).
    assert!(!json.contains("\"disc_hash\""), "{json}");
    assert!(!json.contains("\"vid_available\""), "{json}");
    let body = issue_body(
        &disc,
        &DiscSummary {
            file_count: 1,
            total_bytes: 10,
            skipped: Vec::new(),
        },
        "QUJD",
    )
    .0;
    // An encrypted disc scanned without keys is not "no AACS".
    assert!(!body.contains("No AACS on this disc"), "{body}");
    assert!(body.contains("scanned without keys"), "{body}");
    assert!(json.contains("\"aacs_present\": true"), "{json}");
}

// No titles is "no title picked", not playlist 0.
#[test]
fn an_empty_title_list_picks_no_playlist() {
    let sel = selection_json(&disc_with(None));
    assert!(sel.contains("\"picked_playlist_id\": null"), "{sel}");
}

// aacs_diag's normalisation: a bare, prefixed or padded hash is keyed `0x<hash>`; a blank
// one is no hash; the VID flag follows the bytes.
#[test]
fn the_disc_hash_is_normalised_to_its_keydb_row_form() {
    let want = Some("0xaabbccddeeff00112233445566778899aabbccdd".to_string());
    for raw in [
        "aabbccddeeff00112233445566778899aabbccdd",
        "0xaabbccddeeff00112233445566778899aabbccdd",
        "  0xaabbccddeeff00112233445566778899aabbccdd \n",
    ] {
        let d = aacs_diag(&disc_with(Some(aacs_with_secrets(raw))));
        assert_eq!(d.disc_hash, want, "{raw:?}");
    }
    for blank in ["", "  ", "0x"] {
        let d = aacs_diag(&disc_with(Some(aacs_with_secrets(blank))));
        assert!(d.captured && d.disc_hash.is_none(), "{blank:?}");
    }
    let d = aacs_diag(&disc_with(Some(aacs_with_secrets("0xab"))));
    assert!(d.vid_available);
    assert_eq!(d.mkb_version, Some(77));
    assert_eq!(d.bus_encryption, Some(true));
    let none = libfreemkv::test_util::aacs_state()
        .volume_id([0; 16])
        .bus_encryption(false)
        .mkb_version(None)
        .disc_hash("0xab")
        .build();
    let d = aacs_diag(&disc_with(Some(none)));
    assert!(!d.vid_available);
    assert_eq!(d.mkb_version, None);
    assert_eq!(d.bus_encryption, Some(false));
}

/// L10: the bundle names freemkv's own version, not libfreemkv's label.
#[test]
fn selection_json_names_the_freemkv_version() {
    let sel = selection_json(&disc_with(None));
    let want = format!("\"freemkv\": \"{}\",", env!("CARGO_PKG_VERSION"));
    assert!(sel.contains(&want), "{sel}");
    assert!(sel.contains("\"libfreemkv\": "), "{sel}");
}

// Every text rendering of `secret` a capture could leak: hex, base64 (padded
// or not) and decimal byte arrays in Debug (`1, 2`) and JSON (`1,2`) spacing.
fn encodings(secret: &[u8]) -> Vec<String> {
    let dec: Vec<String> = secret.iter().map(|b| b.to_string()).collect();
    let b64 = base64_encode(secret);
    vec![
        secret.iter().map(|b| format!("{b:02x}")).collect(),
        b64.trim_end_matches('=').to_string(),
        b64,
        dec.join(", "),
        dec.join(","),
    ]
}

// The fixture image scanned as a disc (its AACS state from the image's Unit_Key_RO.inf).
fn scan(img: &libfreemkv::test_util::EncryptedBdImage) -> Disc {
    use libfreemkv::SectorSource;
    let mut src = img.source();
    let cap = src.capacity_sectors();
    Disc::scan_image(&mut src, cap, &libfreemkv::ScanOptions::default()).expect("scan")
}

// A keydb-like source that knows the rip's unit key: its answer needs no samples.
struct KnownKey;

impl libfreemkv::KeySource for KnownKey {
    fn get_unit_keys(
        &self,
        _: &dyn libfreemkv::keysource::ResolveCtx,
    ) -> libfreemkv::error::Result<Vec<libfreemkv::aacs::types::UnitKey>> {
        Ok(vec![libfreemkv::aacs::types::UnitKey::new(
            0,
            SECRET_UNIT_KEY,
        )])
    }
    fn answer_depends_on_samples(&self) -> bool {
        false
    }
}

// The rip's up-front key set over the fixture image (KU §3.3; the type lands at KU-L2).
fn rip_key_set(img: &libfreemkv::test_util::EncryptedBdImage) -> libfreemkv::keys::KeyRing {
    let factory: libfreemkv::KeySourceFactory =
        std::sync::Arc::new(|| vec![Box::new(KnownKey) as Box<dyn libfreemkv::KeySource>]);
    libfreemkv::keys::KeyRing::acquire_for_disc(
        &scan(img),
        &mut img.source(),
        libfreemkv::keys::KeyScope::WholeDisc,
        &factory,
        libfreemkv::keys::AcquireOptions::default(),
        &libfreemkv::Ctx::default(),
    )
    .expect("the known key is proven on the stream")
    .keys
}

/// FK9 (KU design §3.3, §7.3): the `--share` capture of a disc whose stream is
/// encrypted under the rip's key leaks no key, raw VID or MKB bytes into any file
/// it writes, any console line, or the issue title and body.
#[test]
fn bug_report_capture_leaks_no_key_vid_or_mkb() {
    use libfreemkv::aacs::mkb::AacsVersion;
    use libfreemkv::test_util::{BdFile, decrypt_unit, encrypted_bd_image, unit_key_ro};

    let uk_ro = unit_key_ro(AacsVersion::V10, &[[0x42; 16]], &[1]);
    let img = encrypted_bd_image(
        &[
            BdFile::new("BDMV/PLAYLIST/00000.mpls", 3, None),
            BdFile::new("BDMV/CLIPINF/00001.clpi", 3, None),
            BdFile::new("BDMV/STREAM/00001.m2ts", 6, Some(SECRET_UNIT_KEY)),
        ],
        &uk_ro,
    );
    // The key is live: it opens the stream's first aligned unit (CPI masked).
    let at = img.files[2].0 as usize * 2048;
    let mut unit = img.image[at..at + 6144].to_vec();
    assert_ne!(unit, img.plain[at..at + 6144], "the stream is encrypted");
    decrypt_unit(&mut unit, &SECRET_UNIT_KEY);
    let mask = |u: &[u8]| -> Vec<u8> {
        let mut u = u.to_vec();
        u.chunks_mut(192).for_each(|p| p[0] &= 0x3F);
        u
    };
    assert_eq!(mask(&unit), mask(&img.plain[at..at + 6144]));

    // §3.3: the rip's key set over the same image, holding the known key and proven on
    // the stream, exists in memory while the capture runs (and the capture never sees it).
    let set = rip_key_set(&img);
    let status = set.status();
    assert_eq!((status.keyed, status.proven), (1, 1), "{status:?}");
    let scanned = scan(&img);
    let mut reader = set
        .whole_disc_reader(&scanned, img.source(), None)
        .expect("the set is for this image");
    let mut unit = vec![0u8; 6144];
    reader
        .read_sectors(img.files[2].0, 3, &mut unit, false)
        .expect("the set's key opens the stream");
    assert_eq!(mask(&unit), mask(&img.plain[at..at + 6144]));

    let disc = disc_with(Some(aacs_with_secrets(
        "0x1111111111111111111111111111111111111111",
    )));
    let dir = std::env::temp_dir().join(format!("fmkv-fk9-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    // A directory where the clip info would go: one skipped-file console line.
    std::fs::create_dir_all(dir.join("BDMV/CLIPINF/00001.clpi")).expect("blocker");
    let c = match capture(&disc, &mut img.source(), &dir, false) {
        Ok(c) => c,
        Err(lines) => panic!("capture failed: {lines:?}"),
    };
    assert_eq!(c.stderr.len(), 1, "{:?}", c.stderr);
    assert_eq!(c.stdout.len(), 2, "{:?}", c.stdout);

    let read = |n: &str| std::fs::read(dir.join(n)).expect("artifact");
    let text = |n: &str| String::from_utf8(read(n)).expect("utf-8");
    let mut texts: Vec<(String, String)> = vec![
        ("selection.json".into(), text("selection.json")),
        ("aacs.json".into(), text("aacs.json")),
        ("issue title".into(), c.title.clone()),
        ("issue body".into(), c.body.clone()),
    ];
    for (i, line) in c.stdout.iter().chain(&c.stderr).enumerate() {
        texts.push((format!("console line {i}"), line.clone()));
    }
    let structure = [("BDMV/PLAYLIST/00000.mpls", read("BDMV/PLAYLIST/00000.mpls"))];
    let _ = std::fs::remove_dir_all(&dir);

    let secrets: [(&str, &[u8]); 4] = [
        ("unit key", &SECRET_UNIT_KEY),
        ("raw Volume ID", &SECRET_VID),
        ("MKB bytes", &SECRET_MKB),
        ("uk_ro bytes", &[0x12, 0x34, 0x56]),
    ];
    for (what, secret) in secrets {
        for (name, t) in &texts {
            let lower = t.to_ascii_lowercase();
            // Hex in either case; base64 and decimal are case-exact.
            for (i, enc) in encodings(secret).into_iter().enumerate() {
                let hit = if i == 0 {
                    lower.contains(&enc)
                } else {
                    t.contains(&enc)
                };
                assert!(!hit, "{what} leaked into {name} as {enc:?}");
            }
        }
        for (name, data) in &structure {
            let hit = data.windows(secret.len()).any(|w| w == secret);
            assert!(!hit, "{what} bytes leaked into {name}");
        }
    }
}

/// FK9 structural half: the capture API is handed only a `Disc` and a raw reader,
/// never the rip's key set (KU design §3.3).
#[test]
fn the_capture_api_takes_no_key_set() {
    let src = include_str!("disc_capture.rs");
    let api = &src[..src.find("#[cfg(test)]").expect("test module")];
    for banned in ["KeyRing", "KeyFetch", "DecryptKeys"] {
        assert!(!api.contains(banned), "disc_capture API names {banned}");
    }
}
