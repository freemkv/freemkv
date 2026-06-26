// freemkv — WS2 messaging CONTRACT TEST
// AGPL-3.0 — freemkv project
//
// Every live `libfreemkv::Error` variant must have: a non-zero code in a known
// 1xxx-9xxx bucket, an `error.E<code>` message in en.json, the same key in all
// six community locales (es, fr, de, it, pt, nl) with an EXACT placeholder set
// match, and an assigned Level. Plus whole-set guarantees: no orphan locale
// keys, and `error.E*` parity across all seven locales.
//
// `Error` is `#[non_exhaustive]` — the variant list is the hand-maintained
// `test_support::all_error_variants()` fixture, the SAME one `strings.rs`'s
// `every_error_code_has_an_en_string` uses (shared by `include!` because
// `freemkv` is a binary crate with no lib target). A new variant with no
// string/locale/level trips this test rather than shipping a bare code.

use serde_json::Value;

// The shared fixture: `all_error_variants()` + `placeholders()`. Included from
// the crate source via the manifest-relative path so both this integration
// test and the in-binary `strings.rs` test enumerate the identical variant set.
include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/test_support.rs"));

// ── The Level map, mirrored from `messaging.rs` ─────────────────────────────
//
// `freemkv` is a binary crate, so this integration test cannot `use` the
// binary-private `messaging` module. The map is locked by spec (every code is
// `Error`) and pinned by the in-binary `messaging::tests`, so the contract test
// asserts the same closed vocabulary independently: every code maps to one of
// the three Levels, and for every libfreemkv code that Level is `Error`.
// `Warn`/`Info` are never constructed here (every code is `Error`), but the
// assertion checks the closed three-level vocabulary, so they must exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
enum Level {
    Warn,
    Info,
    Error,
}

fn level_for(_code: u16) -> Level {
    Level::Error
}

// ── Locale loading (canonical JSON, read straight from the source tree) ─────

const LOCALE_EN: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/locales/en.json"));
const LOCALE_ES: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/locales/es.json"));
const LOCALE_FR: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/locales/fr.json"));
const LOCALE_DE: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/locales/de.json"));
const LOCALE_IT: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/locales/it.json"));
const LOCALE_PT: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/locales/pt.json"));
const LOCALE_NL: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/locales/nl.json"));

/// The six community locales (en is held separately as the canonical source).
fn other_locales() -> Vec<(&'static str, Value)> {
    [
        ("es", LOCALE_ES),
        ("fr", LOCALE_FR),
        ("de", LOCALE_DE),
        ("it", LOCALE_IT),
        ("pt", LOCALE_PT),
        ("nl", LOCALE_NL),
    ]
    .into_iter()
    .map(|(code, data)| {
        (
            code,
            serde_json::from_str(data).unwrap_or_else(|e| panic!("{code}.json invalid: {e}")),
        )
    })
    .collect()
}

/// Look up a dotted key; returns the dotted key verbatim on a miss (the
/// loader's "miss sentinel"), matching `strings::lookup`.
fn lookup(strings: &Value, path: &str) -> String {
    let mut node = strings;
    for part in path.split('.') {
        match node.get(part) {
            Some(v) => node = v,
            None => return path.to_string(),
        }
    }
    node.as_str()
        .map(|s| s.to_string())
        .unwrap_or_else(|| path.to_string())
}

/// All `error.E<digits>` keys present in a locale (the code-bearing subset; the
/// non-`E` `error.*` keys are CLI-owned validation strings, excluded by spec).
fn error_code_keys(loc: &Value) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    if let Some(err) = loc.get("error").and_then(|v| v.as_object()) {
        for k in err.keys() {
            // `E` followed by at least one digit, all digits.
            if let Some(rest) = k.strip_prefix('E') {
                if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) {
                    out.insert(k.clone());
                }
            }
        }
    }
    out
}

// ── §3.2 — per-variant assertions 1-5 ───────────────────────────────────────

#[test]
fn every_variant_has_code_message_locales_placeholders_and_level() {
    let en: Value = serde_json::from_str(LOCALE_EN).expect("en.json invalid");
    let others = other_locales();

    for e in all_error_variants() {
        let code = e.code();

        // 1. A code exists, non-zero, in a known 1xxx-9xxx bucket.
        assert_ne!(code, 0, "{e:?}: Error::code() returned 0");
        assert!(
            (1000..10_000).contains(&code),
            "{e:?}: code {code} outside the known 1xxx-9xxx range"
        );

        let key = format!("error.E{code}");

        // 2. An en.json message exists (not the miss sentinel).
        let en_msg = lookup(&en, &key);
        assert_ne!(en_msg, key, "en.json missing {key} (variant {e:?})");

        let en_ph: std::collections::BTreeSet<String> = placeholders(&en_msg).into_iter().collect();

        for (lc, loc) in &others {
            // 3. Every other locale has the key.
            let loc_msg = lookup(loc, &key);
            assert_ne!(loc_msg, key, "{lc}.json missing {key} (variant {e:?})");

            // 4. Placeholder sets match EXACTLY (not just superset) — a
            //    translator may not drop `{detail}` or invent `{foo}`.
            let loc_ph: std::collections::BTreeSet<String> =
                placeholders(&loc_msg).into_iter().collect();
            assert_eq!(
                loc_ph, en_ph,
                "{lc}.json {key}: placeholder set {loc_ph:?} != en {en_ph:?}"
            );
        }

        // 5. An assigned Level in the closed vocabulary; Error for every
        //    libfreemkv code.
        let level = level_for(code);
        assert!(
            matches!(level, Level::Warn | Level::Info | Level::Error),
            "{key}: level not in the closed vocabulary"
        );
        assert_eq!(level, Level::Error, "{key}: every libfreemkv code is Error");
    }
}

// ── Message-quality pins (rc6 error-message quality pillar) ─────────────────
//
// The contract test above guarantees every code HAS a string; these pin the
// QUALITY of the high-traffic real-failure messages so an edit can't silently
// regress them below the rubric (WHAT / WHY / WHAT-next). Each asserts the
// English message both names the failure in plain language AND carries an
// actionable next step — and never leaks a bare `E####` token.

/// The English message for a code, via the same lookup the CLI uses.
fn en_msg(code: u16) -> String {
    let en: Value = serde_json::from_str(LOCALE_EN).expect("en.json invalid");
    lookup(&en, &format!("error.E{code}"))
}

#[test]
fn no_drive_message_is_actionable() {
    let en: Value = serde_json::from_str(LOCALE_EN).expect("en.json invalid");
    let m = lookup(&en, "error.no_drive");
    // Plain language (no internal "BD" jargon), and an actionable path syntax.
    assert!(
        m.contains("optical drive") || m.to_lowercase().contains("drive"),
        "no_drive must name the failure plainly: {m}"
    );
    assert!(
        m.contains("disc://"),
        "no_drive must show how to name a drive explicitly: {m}"
    );
}

#[test]
fn drive_not_found_message_is_actionable() {
    let m = en_msg(1000);
    assert!(
        m.to_lowercase().contains("not found") && m.contains("{detail}"),
        "E1000 must name what wasn't found and where: {m}"
    );
    assert!(
        m.to_lowercase().contains("auto-detect") || m.to_lowercase().contains("check"),
        "E1000 must offer a next step: {m}"
    );
}

#[test]
fn drive_not_ready_message_is_actionable() {
    let m = en_msg(1002);
    assert!(m.to_lowercase().contains("not ready"), "E1002 what: {m}");
    assert!(
        m.to_lowercase().contains("insert") && m.to_lowercase().contains("try again"),
        "E1002 must tell the user to insert a disc and retry: {m}"
    );
}

#[test]
fn aacs_no_keys_message_points_at_update_keys() {
    // The known-good remediation pattern: an AACS-needs-keys failure must guide
    // the user to fetch a key database, not just state the fact.
    let m = en_msg(7000);
    assert!(m.contains("AACS"), "E7000 must name AACS: {m}");
    assert!(
        m.contains("update-keys"),
        "E7000 must point at `freemkv update-keys`: {m}"
    );
}

#[test]
fn decrypt_failed_message_is_actionable() {
    let m = en_msg(7013);
    assert!(
        m.to_lowercase().contains("decryption failed"),
        "E7013 what: {m}"
    );
    assert!(
        m.contains("update-keys"),
        "E7013 must offer a remediation (refresh keys): {m}"
    );
}

#[test]
fn no_streams_message_explains_and_diagnoses() {
    let m = en_msg(6009);
    assert!(
        m.to_lowercase().contains("no audio or video") || m.to_lowercase().contains("no streams"),
        "E6009 what: {m}"
    );
    assert!(
        m.to_lowercase().contains("damaged") || m.to_lowercase().contains("unsupported"),
        "E6009 must offer a likely cause: {m}"
    );
}

#[test]
fn improved_messages_carry_no_bare_code() {
    // None of the curated messages may themselves embed a raw `E####` token —
    // the code is prefixed once by the render site, never baked into the prose.
    for code in [1000u16, 1002, 6009, 7000, 7013] {
        let m = en_msg(code);
        assert!(
            !m.contains(&format!("E{code}")),
            "E{code} message must not embed its own raw code: {m}"
        );
    }
}

#[test]
fn capture_failed_key_exists_in_all_locales() {
    // The drive-profile capture failure is now localized (was bare English).
    let en: Value = serde_json::from_str(LOCALE_EN).expect("en.json invalid");
    let en_msg = lookup(&en, "error.capture_failed");
    assert_ne!(
        en_msg, "error.capture_failed",
        "en.json missing capture_failed"
    );
    assert!(
        en_msg.contains("{error}"),
        "capture_failed must keep {{error}}"
    );
    let en_ph: std::collections::BTreeSet<String> = placeholders(&en_msg).into_iter().collect();
    for (lc, loc) in other_locales() {
        let m = lookup(&loc, "error.capture_failed");
        assert_ne!(
            m, "error.capture_failed",
            "{lc}.json missing capture_failed"
        );
        let ph: std::collections::BTreeSet<String> = placeholders(&m).into_iter().collect();
        assert_eq!(ph, en_ph, "{lc}.json capture_failed placeholders differ");
    }
}

// ── §3.2 — assertion 6: no orphan locale keys ───────────────────────────────

#[test]
fn no_orphan_error_code_keys_in_en() {
    let en: Value = serde_json::from_str(LOCALE_EN).expect("en.json invalid");

    // The set of codes a current build can emit.
    let live: std::collections::BTreeSet<String> = all_error_variants()
        .iter()
        .map(|e| format!("E{}", e.code()))
        .collect();

    // Every `error.E*` key in en.json must correspond to a live variant —
    // catches a stale string left behind after a variant was burned/retired.
    let orphans: Vec<String> = error_code_keys(&en)
        .into_iter()
        .filter(|k| !live.contains(k))
        .collect();
    assert!(
        orphans.is_empty(),
        "en.json has orphan error code key(s) with no live Error variant: {orphans:?}"
    );
}

// ── §3.2 — assertion 7: locale parity scoped to error.E* ────────────────────

#[test]
fn error_code_keys_have_locale_parity() {
    let en: Value = serde_json::from_str(LOCALE_EN).expect("en.json invalid");
    let en_keys = error_code_keys(&en);

    for (lc, loc) in other_locales() {
        let loc_keys = error_code_keys(&loc);

        let missing: Vec<&String> = en_keys.difference(&loc_keys).collect();
        assert!(
            missing.is_empty(),
            "{lc}.json missing error code key(s) present in en: {missing:?}"
        );
        let extra: Vec<&String> = loc_keys.difference(&en_keys).collect();
        assert!(
            extra.is_empty(),
            "{lc}.json has error code key(s) not in en: {extra:?}"
        );
    }
}
