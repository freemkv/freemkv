// freemkv — shared test fixtures (WS2), MIT — freemkv project
// Single source of truth for the `Error`-variant list, included via `include!`
// by `tests/messaging_contract.rs`; hand-maintained, drift from `src/error.rs` FAILS the contract test.

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
        // E6013/E6014 were absent from this fixture, so both shipped rendering
        // as a bare code via the `error.generic` fallback. Listed here so the
        // missing-string case is caught like every other variant's.
        Error::UdfNotFilesystem,
        Error::UdfBufferTooSmall,
        Error::DiscTitleRange { index: 0, count: 0 },
        Error::IfoParse,
        Error::MkvInvalid,
        // 1.6.0 audit: E9053/E9054 split read/write sides out of MkvInvalid,
        // E7027 split the disc-wide CSS failure out of CssKeyMissing — one
        // code was carrying two conditions and total failure read as success.
        Error::MkvSourceInvalid,
        Error::MkvUnencodable,
        Error::MkvLacingInvalid,
        Error::CssNoDiscKey,
        // E7028/E7029/E7030 split key-SOURCE failures out of E7022: "the key
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
        // describes. Contract-checked here like every other variant — codes
        // NOT listed (E6013/E6014 above) shipped as a bare number.
        Error::ImageTruncated {
            have: 1_024,
            want: 4_096,
        },
        Error::ImageEndsBeforeRead {
            lba: 2_048,
            have: 1_024,
            want: 4_096,
        },
        // E6021/E6022 carry a path list / an image path as `{detail}`.
        Error::BusStreamUnmapped {
            files: "/BDMV/STREAM/00002.m2ts (E6000: 12345)".into(),
        },
        Error::ImageScoped { path: p() },
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
        Error::AacsKeyFileUnreadable,
        Error::AacsNoUsableHostCert,
        Error::WholeDiscKeyMissing,
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
        // E9019/E9024/E9025 used to be excluded as "caught by pipe.rs preflight
        // instead" — the wrong rule; see the block at the end. Enumerated now.
        Error::DirNotEmpty,
        Error::DirInsufficientSpace {
            required: 0,
            available: 0,
        },
        Error::DirNameCollision { host: p() },
        Error::DirWriteFailed { errno: Some(28) },
        // 1.6.1: dir:// SOURCE errors, the image writer, and the seam gates.
        // These were missing, so E9059-E9070 had no locale entry and a user
        // hit by one saw a bare "Error: E9061" — a gate must list ALL of them.
        Error::ShortImageRead {
            lba: 0,
            expected: 0,
            got: 0,
        },
        Error::EmptyImage,
        Error::DirImageSsifUnsupported,
        Error::DirImagePlacement { path: p() },
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
        Error::StreamClosed,
        Error::StreamHeaderWritten,
        // This list must enumerate EVERY code libfreemkv publishes, not just
        // what the CLI can reach: two tests read a missing entry oppositely
        // (hidden gap vs. stale string), which let E9056/E9057 ship string-less.
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
        // CC-F1a (stop-design-v5 §6 R7.4): the shared cascade's two codes.
        Error::AacsVidNeedsDisc,
        Error::TimedOut { op: "verify" },
        // MPG-F1 (mpg-output-design v5 §8): the mpg:// cascade's one code, E9074.
        Error::MpgNoVideoTrack,
        // mpg:// end of input left access units unwritten, E9075.
        Error::MpgUnpacketized,
        // Engine remux/preflight refusals, E9077-E9084.
        Error::RemuxVerifyFailed {
            kind: libfreemkv::RemuxVerifyKind::RuntimeMismatch {
                have_secs: 0.0,
                want_secs: 1.0,
            },
            path: p(),
        },
        Error::MuxIncomplete { title: 1 },
        Error::RemuxStagingInvalid,
        Error::StagedCopySizeMismatch { have: 0, want: 1 },
        Error::WorkerLost { op: "copy" },
        Error::StreamLanguageUnknown { tag: p() },
        Error::RemuxTargetExists { path: p() },
        // A mux batch of zero sectors, E9085.
        Error::MuxBatchSectorsZero,
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
