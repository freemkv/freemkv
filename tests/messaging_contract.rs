// freemkv — WS2 messaging CONTRACT TEST
// MIT — freemkv project
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

// ── The Level map ────────────────────────────────────────────────────────────
//
// This used to be a LOCAL copy of `messaging.rs`'s map — a stub `level_for`
// that always returned `Level::Error` regardless of its argument (the
// parameter was unused), asserted below against the literal `Level::Error` it
// had just returned. That assertion could not fail no matter what the REAL
// `messaging::level_for` in `src/messaging.rs` did: a future change mapping
// some code to `Warn`/`Info` — the exact regression the comment above this
// used to claim it caught — would sail through. `messaging` is now declared
// `pub` in `lib.rs` (the same "declared in both `main.rs` and `lib.rs`"
// pattern already used for `keydb_fetch`) specifically so this test can call
// the real function instead.
use freemkv::messaging::{Level, level_for};

// ── Locale loading (canonical JSON, from the bundled freemkv-i18n crate) ────
//
// The locale files now live in the shared `freemkv-i18n` crate, compiled in
// via its build.rs. Reading them through `bundled_locale_json` keeps this
// contract test working wherever the dependency is resolved from (local path
// patch or the pinned git tag) — a sibling-directory `../freemkv-i18n` path
// would only exist in a local checkout, not in CI's registry cache.

fn bundled(code: &str) -> &'static str {
    freemkv_i18n::bundled_locale_json(code)
        .unwrap_or_else(|| panic!("{code} not bundled in freemkv-i18n"))
}

fn locale_en() -> &'static str {
    bundled("en")
}

/// The six community locales (en is held separately as the canonical source).
fn other_locales() -> Vec<(&'static str, Value)> {
    [
        ("es", bundled("es")),
        ("fr", bundled("fr")),
        ("de", bundled("de")),
        ("it", bundled("it")),
        ("pt", bundled("pt")),
        ("nl", bundled("nl")),
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
            if let Some(rest) = k.strip_prefix('E')
                && !rest.is_empty()
                && rest.bytes().all(|b| b.is_ascii_digit())
            {
                out.insert(k.clone());
            }
        }
    }
    out
}

// ── §3.2 — per-variant assertions 1-5 ───────────────────────────────────────

#[test]
fn every_variant_has_code_message_locales_placeholders_and_level() {
    let en: Value = serde_json::from_str(locale_en()).expect("en.json invalid");
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
        // Exhaustive match over the closed vocabulary: a runtime `matches!` here
        // is a tautology (every arm is covered), so guard at compile time
        // instead — a future `Level` variant forces this test to be updated.
        match level {
            Level::Warn | Level::Info | Level::Error => {}
        }
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
    let en: Value = serde_json::from_str(locale_en()).expect("en.json invalid");
    lookup(&en, &format!("error.E{code}"))
}

#[test]
fn no_drive_message_is_actionable() {
    let en: Value = serde_json::from_str(locale_en()).expect("en.json invalid");
    let m = lookup(&en, "error.no_drive");
    // A missing key returns the dotted sentinel ("error.no_drive"), which
    // happens to contain "drive" — assert presence first so a dropped key
    // reports the true cause, not a vaguer content failure downstream.
    assert_ne!(m, "error.no_drive", "en.json missing error.no_drive");
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
    let en: Value = serde_json::from_str(locale_en()).expect("en.json invalid");
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

// ── Where "the set of error codes" actually lives ───────────────────────────
//
// It lives in ONE place: the `pub const E_*: u16` declarations in libfreemkv's
// `src/error.rs`. Nothing else knows. `Error` is `#[non_exhaustive]`, so no
// amount of reflection on the compiled dependency will enumerate it, and
// libfreemkv exports no machine-readable list today.
//
// The consequence is that every consumer has grown its OWN copy of the list,
// and every copy has silently gone stale in turn:
//
//   * this crate's `all_error_variants()` fixture — 116 variants against
//     libfreemkv's 127, missing E5001, E6016-E6018, E9019, E9024, E9025,
//     E9055-E9058. Twice before that: E6013/E6014, then E9059-E9070.
//   * freemkv-i18n's `src/error_codes.rs` — generated by its
//     `ci/sync-error-codes.sh`, and correct precisely because it is generated.
//   * libfreemkv's own `all_error_code_constants_are_unique`, which as of this
//     writing lists 109 of its 128 constants, so the uniqueness claim does not
//     cover the other 19.
//
// A hand-copied list of another crate's constants does not merely risk being
// wrong; in this codebase it has been wrong every single time. So the two
// tests below stop hand-maintaining an answer and PARSE THE DECLARATIONS,
// exactly as freemkv-i18n's `libfreemkv_code_list_has_not_drifted` does. The
// fixture stays hand-written — it must be, because each entry is a constructed
// value with real fields — but its CODE SET is now derived, not asserted.
//
// Both tests skip, loudly, with no sibling checkout: resolved from a git tag,
// libfreemkv lands in the cargo cache at an unstable path. That is not a
// coverage hole. `.github/workflows/ci.yml` checks every sibling out into
// `../<crate>` for every job in this repo — that is the whole reason the CI
// config exists in that shape — and it is the standard local layout too.

/// Libfreemkv's declared error codes, as (code, constant name), parsed from a
/// sibling checkout of its `src/error.rs`. `None` when no sibling exists.
fn libfreemkv_declared_codes() -> Option<std::collections::BTreeMap<u32, String>> {
    let source =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../libfreemkv/src/error.rs");
    let Ok(text) = std::fs::read_to_string(&source) else {
        eprintln!(
            "no sibling libfreemkv checkout at {}; skipping error-code drift check",
            source.display()
        );
        return None;
    };

    // `pub const E_NAME: u16 = 1234;` — the one declaration form libfreemkv
    // uses. Matched on the trimmed line so a mention inside a doc comment or a
    // function body cannot be counted as a declaration.
    let mut declared = std::collections::BTreeMap::new();
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix("pub const E_") else {
            continue;
        };
        let Some((name, value)) = rest.split_once(": u16 = ") else {
            continue;
        };
        let Some(digits) = value.strip_suffix(';') else {
            continue;
        };
        let Ok(code) = digits.trim().parse::<u32>() else {
            continue;
        };
        declared.insert(code, format!("E_{name}"));
    }

    // A parser that quietly matched nothing would turn both tests below into
    // no-ops that pass forever — the exact shape of defect they exist to catch.
    // The real count is ~128; 100 is a floor, not a target.
    assert!(
        declared.len() > 100,
        "parsed only {} error-code constants from {} — the declaration form changed and these \
         tests stopped checking anything",
        declared.len(),
        source.display()
    );
    Some(declared)
}

/// The subset of declared codes that an `Error` value can actually carry: the
/// constants named on the right-hand side of an arm in `Error::code()`.
///
/// The distinction is real and load-bearing. A constant with no `code()` arm is
/// a code libfreemkv emits by NUMBER — into a log line, an accounting counter,
/// a bug report — without ever building an `Error` for it. E6019
/// (`E_UDF_NO_USABLE_EXTENT`) is documented upstream as exactly that: "consumed
/// by the caller, not returned to it". The fixture below cannot enumerate such
/// a code (there is no variant to construct) and must not be asked to, but
/// en.json absolutely should still carry a sentence for it — a number a user
/// can read is a number a user can be confused by.
fn codes_carried_by_a_variant(text: &str) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    for line in text.lines() {
        let Some(idx) = line.find("=> E_") else {
            continue;
        };
        let name: String = line[idx + 3..]
            .chars()
            .take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == '_')
            .collect();
        out.insert(name);
    }
    out
}

/// Catches a stale `error.E*` string left in en.json after its code was
/// retired — the mutation being: delete a constant from libfreemkv's
/// `error.rs` and leave its message behind, where 29 locales' worth of
/// translators go on maintaining a sentence no build can ever print.
///
/// THIS TEST USED TO ANSWER A DIFFERENT QUESTION. It compared en.json against
/// `all_error_variants()` and called anything else an orphan — reading the
/// fixture as "the set of codes that may legitimately have a string" while
/// `every_variant_has_code_message_locales_placeholders_and_level` reads the
/// same list as "the set that must have one". Two opposite readings of one
/// hand-maintained list, and the list was missing eleven codes. So the first
/// reading hid a real gap for releases — E9056/E9057, the codes that tell a
/// user the rip could NOT be confirmed written to disk, reached them as the
/// literal text `error.E9056` while they decided whether to delete the source
/// — and the second reading then reported the party who FIXED that gap as
/// having introduced eleven stale strings. It failed in whichever direction
/// the fixture was wrong, and it was always the fixture that was wrong.
///
/// It asks libfreemkv now. An `error.E*` key is legitimate if libfreemkv
/// DECLARES that code — not if this crate happens to construct it. Three of
/// the eleven (E9019/E9024/E9025) are `dir://` flag gates that `pipe.rs`
/// intercepts in preflight with its own strings, so no CLI path will ever
/// render them; they are still `pub` codes of a library with other consumers,
/// and "unreachable from `pipe.rs`" is not grounds to delete a message. Only
/// "no longer a code" is, and that is what this now tests.
#[test]
fn en_json_has_no_string_for_a_code_libfreemkv_does_not_declare() {
    let Some(declared) = libfreemkv_declared_codes() else {
        return;
    };
    let en: Value = serde_json::from_str(locale_en()).expect("en.json invalid");

    let orphans: Vec<String> = error_code_keys(&en)
        .into_iter()
        .filter(|k| {
            k[1..]
                .parse::<u32>()
                .is_ok_and(|c| !declared.contains_key(&c))
        })
        .collect();
    assert!(
        orphans.is_empty(),
        "en.json has error string(s) for code(s) libfreemkv no longer declares: {orphans:?}. \
         Retire them from all 29 locales in freemkv-i18n."
    );
}

/// Every code libfreemkv can put inside an `Error` must appear in
/// `all_error_variants()`, and the fixture must claim no code that libfreemkv
/// has retired.
///
/// The mutation this catches: add an error variant to libfreemkv, ship it, and
/// forget this fixture. A forgotten variant is not merely untested here — it
/// is INVISIBLE. Every other assertion in this file iterates the fixture, so a
/// variant absent from it has no English-string requirement, no locale-parity
/// requirement, no placeholder requirement and no Level requirement, and the
/// first person to notice is a user reading `error.E9056` off their terminal.
/// That has now happened three times (E6013/E6014, E9059-E9070, and the eleven
/// enumerated at the end of `src/test_support.rs`), each time found by someone
/// auditing a different crate for an unrelated reason.
///
/// Codes with no `code()` arm are excluded by construction — see
/// `codes_carried_by_a_variant`. Their strings are still guarded, from the
/// other side, by `en_json_has_no_string_for_a_code_libfreemkv_does_not_declare`
/// above and by freemkv-i18n's own generated-list tests.
#[test]
fn fixture_enumerates_every_error_code_a_variant_can_carry() {
    let Some(declared) = libfreemkv_declared_codes() else {
        return;
    };
    let text = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../libfreemkv/src/error.rs"),
    )
    .expect("sibling error.rs was readable a moment ago");
    let carried = codes_carried_by_a_variant(&text);

    let enumerated: std::collections::BTreeSet<u32> = all_error_variants()
        .iter()
        .map(|e| u32::from(e.code()))
        .collect();

    let missing: Vec<String> = declared
        .iter()
        .filter(|(code, name)| carried.contains(*name) && !enumerated.contains(code))
        .map(|(code, name)| format!("E{code} ({name})"))
        .collect();
    assert!(
        missing.is_empty(),
        "libfreemkv has {} error variant(s) that all_error_variants() does not enumerate, so \
         nothing in this file checks that they have a message: {:?}. Add them to \
         src/test_support.rs and write their strings in freemkv-i18n (all 29 locales).",
        missing.len(),
        missing
    );

    let retired: Vec<u32> = enumerated
        .iter()
        .copied()
        .filter(|code| !declared.contains_key(code))
        .collect();
    assert!(
        retired.is_empty(),
        "all_error_variants() enumerates code(s) libfreemkv no longer declares: {retired:?}. \
         Drop them here, then retire their strings from freemkv-i18n's 29 locales."
    );
}

// ── §3.2 — assertion 7: locale parity scoped to error.E* ────────────────────

#[test]
fn error_code_keys_have_locale_parity() {
    let en: Value = serde_json::from_str(locale_en()).expect("en.json invalid");
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
