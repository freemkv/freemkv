// freemkv — shared test fixtures (WS2)
// MIT — freemkv project
//
// Single source of truth for the enumerated `Error`-variant list and the
// placeholder extractor, shared between `strings.rs`'s
// `every_error_code_has_an_en_string` unit test and the
// `tests/messaging_contract.rs` integration test. Because `freemkv` is a
// binary crate (no lib target), this file is shared by `include!` rather than
// as a normal module: both the in-binary test module and the integration test
// `include!` it, so the two lists can never drift.
//
// `Error` is `#[non_exhaustive]`, so this list is hand-maintained — that is the
// design: a new variant with no string/locale/level trips the contract test
// rather than shipping a bare code.
//
// Hand-maintained does not mean unchecked. See
// `fixture_enumerates_every_libfreemkv_error_code` in
// `tests/messaging_contract.rs`: it compares the code set below against the
// `pub const E_*` declarations in libfreemkv's own `src/error.rs`, in both
// directions, so a variant added upstream and forgotten here FAILS rather than
// silently dropping out of every assertion that iterates this list.

/// Construct one instance of every error code `libfreemkv` publishes.
///
/// NOT "every code the CLI can surface" — that narrower reading is what left
/// eleven codes unchecked; see the long note at the end of the list.
pub fn all_error_variants() -> Vec<libfreemkv::Error> {
    use libfreemkv::Error;
    let p = || "p".to_string();
    vec![
        Error::DeviceNotFound { path: p() },
        Error::DevicePermission { path: p() },
        Error::DeviceNotReady { path: p() },
        Error::DeviceResetFailed { path: p() },
        Error::ScsiInterfaceUnavailable { path: p() },
        Error::DeviceLocked { path: p(), kr: 0 },
        Error::IoKitPluginFailed { path: p(), kr: 0 },
        Error::UnsupportedDrive {
            vendor_id: p(),
            product_id: p(),
            product_revision: p(),
        },
        Error::ProfileParse,
        Error::UnsupportedPlatform { target: p() },
        Error::PlatformNotImplemented { platform: p() },
        Error::UnlockFailed,
        Error::SignatureMismatch {
            expected: [0; 4],
            got: [0; 4],
        },
        Error::ScsiError {
            opcode: 0,
            status: 0,
            sense: None,
        },
        Error::InvalidCdbLength { len: 0, max: 0 },
        Error::IoError {
            source: std::io::Error::from_raw_os_error(13),
        },
        Error::DiscRead {
            sector: 0,
            status: None,
            sense: None,
        },
        Error::Halted,
        Error::MplsParse,
        Error::ClpiParse,
        Error::UdfNotFound { path: p() },
        // E6013/E6014 were absent from this fixture, so the contract test never
        // asked for their strings and both shipped rendering as a bare code via
        // the `error.generic` fallback. Listed here so the missing-string case
        // is caught the way every other variant's is.
        Error::UdfNotFilesystem,
        Error::UdfBufferTooSmall,
        Error::DiscTitleRange { index: 0, count: 0 },
        Error::IfoParse,
        Error::MkvInvalid,
        // Added by the 1.6.0 audit. E9053/E9054 split the read and write sides
        // out of MkvInvalid, and E7027 split the disc-wide CSS failure out of
        // CssKeyMissing — in both cases because one code was carrying two
        // conditions and a total failure was being reported as success.
        Error::MkvSourceInvalid,
        Error::MkvUnencodable,
        Error::MkvLacingInvalid,
        Error::CssNoDiscKey,
        // E7028/E7029/E7030 split the key-SOURCE failures out of E7022 for the
        // same reason: one code was carrying two conditions, and "the key
        // service could not answer" was being reported as "this disc has no
        // key" — a seven-hour HTTP 502 outage read as a missing VUK.
        Error::KeyServiceUnavailable,
        Error::KeyServiceUnauthorized,
        Error::KeyServiceRateLimited,
        Error::MuxHeaderBufferExceeded { bytes: 0 },
        Error::NoStreams,
        // `pid` is rendered into the message as `{detail}` (`E6014: 0x1011`), so
        // pick a realistic non-zero PID rather than 0.
        Error::SelectionPidUnknown { pid: 0x1011 },
        Error::MapfileInvalid { kind: "hex" },
        // E6015: a resume against an image shorter than the recovery data
        // describes. Listed here so its locale string is contract-checked like
        // every other variant's — the codes that were NOT listed (E6013/E6014
        // above) are exactly the ones that shipped as a bare number.
        Error::ImageTruncated {
            have: 1_024,
            want: 4_096,
        },
        Error::AacsNoKeys,
        Error::AacsCertShort,
        Error::AacsAgidAlloc,
        Error::AacsCertRejected,
        Error::AacsCertRead,
        Error::AacsCertVerify,
        Error::AacsKeyRead,
        Error::AacsKeyRejected,
        Error::AacsKeyVerify,
        Error::AacsVidRead,
        Error::AacsVidMac,
        Error::AacsDataKey,
        Error::DecryptFailed,
        Error::CssAuthFailed,
        Error::AacsHostCertRejected,
        Error::AacsRawReadUnsupported,
        Error::AacsVidUnavailable,
        Error::AacsMkUnavailable,
        Error::AacsVukNotInKeydb,
        Error::DriveProfileMissing,
        Error::VidCdbUnavailable,
        Error::NoDiscKey { disc_hash: p() },
        Error::CssKeyMissing,
        Error::AacsNoHostCert { path: p() },
        Error::AacsBusKeyUnavailable,
        Error::FmtsKeyMissing,
        Error::KeydbConnect { host: p() },
        Error::KeydbHttp { status: 0 },
        Error::KeydbInvalid,
        Error::KeydbWrite { path: p() },
        Error::KeydbParse,
        Error::KeydbLoad { path: p() },
        Error::KeydbUnsupportedScheme { scheme: p() },
        Error::KeydbTooManyRedirects,
        Error::StreamReadOnly,
        Error::StreamWriteOnly,
        Error::StreamUrlInvalid { url: p() },
        Error::StreamUrlMissingPath { scheme: p() },
        Error::StreamUrlMissingPort { addr: p() },
        Error::NetworkAddrBlocked { addr: p() },
        Error::MuxEmpty,
        Error::Mp4NoVideoTrack,
        Error::Mp4Invalid,
        Error::Mp4MissingCodecPrivate,
        Error::PesFrameTooLarge { size: 0 },
        Error::PesInvalidMagic,
        Error::PesTrackTooLarge { track: 0 },
        Error::IsoTooLarge { path: p() },
        Error::NoMetadata,
        Error::DiscUrlNotDirect,
        Error::HevcParamParse,
        Error::MuxTrackRange {
            track: 0,
            tracks: 0,
        },
        Error::Fmp4Unimplemented,
        Error::DemuxThreadPanicked,
        Error::PipelineJoinTimeout,
        Error::PipelineConsumerPanicked,
        Error::SweepConsumerGone,
        Error::PipelineConsumerGone,
        Error::DiscCapacityOverflow,
        Error::ExtentNotUnitAligned,
        Error::M2tsPacketMalformed,
        Error::DiscCapacityMalformed,
        // dir:// extraction errors, produced inside `Disc::extract_tree`.
        //
        // This block used to end with a note explaining that E9019/E9024/E9025
        // were "intentionally NOT enumerated here" because `pipe.rs` catches
        // those three combinations in preflight with its own CLI-owned strings
        // (`error.raw_iso_only`, `error.multipass_iso_only`,
        // `error.dir_source_unsupported`) before libfreemkv is ever asked. That
        // is true, and it was still the wrong rule — see the block at the end of
        // this list. They are enumerated now.
        Error::DirNotEmpty,
        Error::DirInsufficientSpace {
            required: 0,
            available: 0,
        },
        Error::DirNameCollision { host: p() },
        Error::DirWriteFailed { errno: Some(28) },
        // 1.6.1: everything a `dir://` SOURCE can raise, plus the image writer
        // and the seam gates. These were missing, so the contract test below
        // passed while E9059-E9070 had no locale entry at all and a user hit
        // by one saw a bare "Error: E9061" — an untranslated code with no
        // explanation, on the headline feature of the release. A fixture that
        // lists the variants is only a gate if it lists ALL of them.
        Error::ShortImageRead {
            lba: 0,
            expected: 0,
            got: 0,
        },
        Error::EmptyImage,
        Error::DirImageSsifUnsupported,
        Error::DirImagePlacement { path: p() },
        Error::DirImageEncrypted,
        Error::DirImageUnsupportedTree,
        Error::DirImageFileChanged { path: p() },
        Error::DirImageTooLarge,
        Error::DirNameTooLong { path: p() },
        Error::DirImageFanout { path: p() },
        Error::SeamPlanDroppedMost {
            dropped: 0,
            written: 0,
        },
        Error::SinkWroteNothing,
        // ── The eleven codes this fixture published a string for and never
        //    asked about ─────────────────────────────────────────────────────
        //
        // WHY this block exists, in full, because the same hole has now been
        // punched three times (E6013/E6014, then E9059-E9070, now these):
        //
        // Two tests read this list and read it as two DIFFERENT things.
        // `every_error_code_has_an_en_string` /
        // `every_variant_has_code_message_locales_placeholders_and_level` treat
        // it as "codes that need a string" — so anything missing here is simply
        // never asked for. `no_orphan_error_code_keys_in_en` treats it as "the
        // set of codes that legitimately have a string" — so anything missing
        // here is reported as a STALE STRING TO DELETE. One hand-maintained list
        // under two opposite readings: an omission first hides a real gap, then
        // accuses the party who fixed it.
        //
        // That is exactly what happened. freemkv-i18n diffed libfreemkv's 127
        // `pub const E_*` constants against en.json, found eleven with no
        // message at all — E9056/E9057 among them, the two that tell a user the
        // rip could NOT be confirmed written to disk, rendering as the literal
        // text `error.E9056` to someone deciding whether it is safe to delete
        // the source — and wrote strings for all eleven across 29 locales.
        // Whereupon this fixture, 117 variants against libfreemkv's 127 codes,
        // called those eleven strings orphans.
        //
        // The rule is therefore restated, and it is the rule the rest of this
        // file now follows: THIS LIST ENUMERATES EVERY ERROR CODE LIBFREEMKV
        // PUBLISHES, not every code the CLI's own code paths can reach.
        //
        // Eight of the eleven were plain omissions — libfreemkv really does
        // return them, from `mux/disc.rs` and `sector/prefetched.rs` (E5001),
        // `udf.rs` (E6016/E6017/E6018), `mux/mp4/mod.rs` (E9055),
        // `io/writeback_file/{macos,linux}.rs` (E9056/E9057) and `identity.rs`
        // (E9058). Nothing but this fixture's incompleteness kept them from
        // being caught by the string check years earlier.
        //
        // Three — E9019/E9024/E9025 — are genuinely unreachable from the CLI:
        // `pipe.rs` rejects those flag/URL combinations in preflight with its
        // own strings. They are still `pub` variants of a `pub` enum in a
        // library with other consumers, they still have a `code()` arm, and a
        // number that can appear in a log line or a bug report is worth a
        // sentence. Excluding them bought nothing and cost the bijection below.
        //
        // The bijection is the point. With this list complete,
        // `every_variant_has_...` (fixture ⊆ en.json) and
        // `no_orphan_error_code_keys_in_en` (en.json ⊆ fixture) together assert
        // fixture == en.json, and freemkv-i18n's own CI asserts en.json ==
        // libfreemkv's constants in both directions. Chained, that is one list,
        // mechanically. `fixture_enumerates_every_libfreemkv_error_code` in
        // tests/messaging_contract.rs closes the loop directly against
        // libfreemkv's source wherever a sibling checkout exists — which, per
        // .github/workflows/ci.yml, is every job in this repo's CI.
        Error::SourceTerminated,
        Error::UdfAdChainTooLong,
        Error::UdfUnrecordedExtent { path: p() },
        Error::UdfEmbeddedData,
        Error::DirRawRejected,
        Error::DirMultipassRejected,
        Error::DirSourceUnsupported,
        Error::Mp4UnknownResolution,
        Error::SyncTimeout,
        Error::SyncWorkerLost,
        Error::DriveInquiryShort,
    ]
}

/// Extract `{word}` placeholders from a format string, matching exactly how
/// `strings::fmt` substitutes them: a balanced single `{...}` with no nested
/// braces. Escaped/doubled braces (`{{`, `}}`) are skipped so a literal
/// `{{val}}` does not register a malformed `{{val}` placeholder. Returns the
/// placeholders in source order (the contract test compares them as a set).
pub fn placeholders(s: &str) -> Vec<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            // Doubled `{{` is an escape, not a placeholder — skip both.
            if bytes.get(i + 1) == Some(&b'{') {
                i += 2;
                continue;
            }
            if let Some(rel_end) = s[i + 1..].find('}') {
                let inner = &s[i + 1..i + 1 + rel_end];
                // A real placeholder has no nested brace inside it.
                if !inner.contains('{') {
                    out.push(format!("{{{}}}", inner));
                }
                i += 1 + rel_end + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}
