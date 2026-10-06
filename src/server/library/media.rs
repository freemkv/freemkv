//! What is in an MKV, for the Library's details dialog: resolution and HDR / Dolby Vision,
//! every audio track's format, channels, lossless and Atmos, the best audio, what the tier
//! lacks, every subtitle, and where the content really ends.
//!
//! Read from the Matroska headers, a bounded look at each track's first frames (Atmos,
//! DTS-HD MA and HDR10+ live only in the bitstream) and the clusters from the last Cues
//! entry to the end (the real last frame). What it cannot establish stays unknown: a
//! `None`, never a guess. It never decides whether a file passes its audit.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{self, BufReader, Read, Seek};
use std::path::Path;

/// Bumped when the reader learns something new, so stored reports are read again.
pub const VERSION: u32 = 1;

/// The media facts of one MKV.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaDetail {
    pub version: u32,
    /// `uhd`, `bluray` or `sd`, from the main video's frame size.
    pub tier: Option<String>,
    pub hdr: Hdr,
    pub video: Vec<VideoTrack>,
    pub audio: Vec<AudioTrack>,
    pub subtitles: Vec<SubtitleTrack>,
    /// Index into `audio` of the best track: lossless, then channels, then Atmos.
    pub best_audio: Option<usize>,
    /// Index into `audio` of the track a player starts with.
    pub default_audio: Option<usize>,
    /// The default track is worse than the best one.
    pub default_not_best: bool,
    /// What a better release of this tier would have: `lossless`, `DV`, `Atmos`, `7.1`, `UHD`.
    pub radar: Vec<String>,
    /// Radar entries this read could not settle.
    pub radar_unknown: Vec<String>,
    pub timeline: Option<Timeline>,
    /// Every header field, per section; dropped from the listing, served on its own.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub raw: Vec<RawSection>,
}

/// The HDR of the main video, with Dolby Vision from any video track.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Hdr {
    /// `sdr`, `hdr10` (PQ), `hlg`, `dv` (Dolby Vision with no fallback layer) or `unknown`.
    pub format: String,
    /// HDR10+ dynamic metadata in the first frame; `None` when no frame was read.
    pub hdr10plus: Option<bool>,
    pub dv: Option<DolbyVision>,
}

/// A Dolby Vision configuration record (`dvcC` / `dvvC`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DolbyVision {
    pub profile: u8,
    pub level: u8,
    /// The base layer's fallback: 1 HDR10, 2 SDR, 4 HLG, 6 Blu-ray HDR10, 0 none.
    pub compat_id: u8,
    pub rpu: bool,
    pub el: bool,
    pub bl: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VideoTrack {
    pub number: u64,
    pub codec: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<f64>,
    pub bit_depth: Option<u8>,
    pub interlaced: Option<bool>,
    /// CICP code points from the Colour element.
    pub transfer: Option<u8>,
    pub primaries: Option<u8>,
    pub matrix: Option<u8>,
    pub max_cll: Option<u32>,
    pub max_fall: Option<u32>,
    pub mastering_max_nits: Option<f64>,
    pub mastering_min_nits: Option<f64>,
    pub dv: Option<DolbyVision>,
    pub hdr10plus: Option<bool>,
    pub title: String,
    pub language: String,
    pub default: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioTrack {
    pub number: u64,
    /// `TrueHD`, `DTS-HD MA`, `DTS-HD HRA`, `DTS Express`, `DTS`, `E-AC-3`, `AC-3`, `PCM`, ...
    pub format: String,
    pub channels: Option<u8>,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u8>,
    pub lossless: Option<bool>,
    /// Atmos object audio; `Some(false)` for a format that cannot carry it.
    pub atmos: Option<bool>,
    pub language: String,
    pub title: String,
    pub default: bool,
    pub forced: bool,
    /// Its title or codec says lossless, but the stream is lossy.
    pub lossy_claim: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SubtitleTrack {
    pub number: u64,
    pub format: String,
    pub language: String,
    pub title: String,
    pub default: bool,
    pub forced: bool,
}

/// The declared length against the content.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Timeline {
    pub declared_secs: Option<f64>,
    pub last_cue_secs: Option<f64>,
    /// Start of the last frame of any track.
    pub last_frame_secs: Option<f64>,
    /// `last_frame - declared`.
    pub delta_secs: Option<f64>,
    /// `ok`, `overrun` (content runs past the declared end), `short`, or `unknown`.
    pub state: String,
}

/// One block of the raw report: a title and its fields in file order.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RawSection {
    pub title: String,
    pub fields: Vec<(String, String)>,
}

/// The detail of a file that is not a readable MKV: nothing to show, nothing to re-read.
pub fn empty() -> MediaDetail {
    MediaDetail {
        version: VERSION,
        hdr: Hdr {
            format: "unknown".into(),
            ..Hdr::default()
        },
        ..MediaDetail::default()
    }
}

/// Read the detail of `path`. `None` when the storage failed (try again later); a file
/// this reader cannot follow gets what it could read, the rest unknown.
pub fn read(path: &Path) -> Option<MediaDetail> {
    match read_file(path) {
        Ok(d) => Some(d),
        Err(e) if super::probe::is_verdict(&e) => Some(empty()),
        Err(_) => None,
    }
}

// How far past the first cluster the first frames are looked for, and how much of each.
const SNIFF_SPAN: u64 = 64 << 20;
const SNIFF_AUDIO_BYTES: usize = 64 << 10;
const SNIFF_AUDIO_BLOCKS: u32 = 4;
const SNIFF_VIDEO_BYTES: usize = 256 << 10;
// The tail walk reads block headers only, from the last indexed cluster to the end.
const TAIL_SPAN: u64 = 1 << 30;
const MAX_BLOCKS: u32 = 200_000;
// The old auditor's tolerance between the declared end and the last frame.
const TIMELINE_TOLERANCE_SECS: f64 = 1.0;

fn read_file(path: &Path) -> io::Result<MediaDetail> {
    let mut r = BufReader::new(std::fs::File::open(path)?);
    let lay = layout(&mut r)?;
    let info = Info::parse(lay.info.as_deref().unwrap_or_default());
    let tracks: Vec<Track> = lay
        .tracks
        .as_deref()
        .map(|b| {
            children(b)
                .filter(|(id, _)| *id == TRACK_ENTRY)
                .map(|(_, body)| Track::parse(body))
                .collect()
        })
        .unwrap_or_default();
    let frames = match lay.first_cluster {
        Some(at) => soft(sniff(&mut r, at, lay.seg_end, &tracks))?,
        None => HashMap::new(),
    };
    let cue = lay.cues.as_deref().and_then(last_cue);
    let last_ticks = match cue {
        Some((_, rel)) => soft(last_block_ticks(
            &mut r,
            lay.seg_start.saturating_add(rel),
            lay.seg_end,
        ))?,
        None => None,
    };
    let secs = |ticks: u64| ticks as f64 * info.scale as f64 / 1e9;
    let mut d = build(&tracks, &frames);
    d.timeline = Some(timeline(
        info.duration.map(|t| t * info.scale as f64 / 1e9),
        cue.map(|(t, _)| secs(t)),
        last_ticks.map(secs),
    ));
    d.raw = std::iter::once(RawSection {
        title: "Segment".into(),
        fields: info.raw,
    })
    .chain(tracks.into_iter().map(|t| RawSection {
        title: format!("Track {} ({})", t.number, kind_name(t.kind)),
        fields: t.raw,
    }))
    .collect();
    Ok(d)
}

// A verdict about the bytes leaves that part unknown; a storage failure is passed up.
fn soft<T: Default>(r: io::Result<T>) -> io::Result<T> {
    match r {
        Err(e) if super::probe::is_verdict(&e) => Ok(T::default()),
        other => other,
    }
}

fn kind_name(kind: u64) -> &'static str {
    match kind {
        1 => "video",
        2 => "audio",
        17 => "subtitles",
        _ => "other",
    }
}

// ── The summary ────────────────────────────────────────────────────────────

fn build(tracks: &[Track], frames: &HashMap<u64, Vec<u8>>) -> MediaDetail {
    let data = |t: &Track| frames.get(&t.number).map(Vec::as_slice);
    let video: Vec<VideoTrack> = tracks
        .iter()
        .filter(|t| t.kind == 1)
        .map(|t| video_track(t, data(t)))
        .collect();
    let audio: Vec<AudioTrack> = tracks
        .iter()
        .filter(|t| t.kind == 2)
        .map(|t| audio_track(t, data(t)))
        .collect();
    let subtitles = tracks
        .iter()
        .filter(|t| t.kind == 17)
        .map(|t| SubtitleTrack {
            number: t.number,
            format: super::probe::codec_name(&t.codec_id),
            language: t.language(),
            title: t.name.clone(),
            default: t.default,
            forced: t.forced,
        })
        .collect();
    let mut d = MediaDetail {
        version: VERSION,
        tier: video.first().and_then(tier),
        hdr: hdr(&video),
        ..MediaDetail::default()
    };
    d.best_audio = best_audio(&audio);
    d.default_audio =
        (!audio.is_empty()).then(|| audio.iter().position(|a| a.default).unwrap_or(0));
    d.default_not_best = match (d.best_audio, d.default_audio) {
        (Some(b), Some(def)) => audio_score(&audio[def]) < audio_score(&audio[b]),
        _ => false,
    };
    (d.radar, d.radar_unknown) = radar(d.tier.as_deref(), &d.hdr, &audio);
    d.video = video;
    d.audio = audio;
    d.subtitles = subtitles;
    d
}

fn tier(v: &VideoTrack) -> Option<String> {
    let (w, h) = (v.width.unwrap_or(0), v.height.unwrap_or(0));
    let t = if h >= 2160 || w >= 3840 {
        "uhd"
    } else if h >= 1080 || w >= 1920 {
        "bluray"
    } else if h > 0 {
        "sd"
    } else {
        return None;
    };
    Some(t.into())
}

fn hdr(video: &[VideoTrack]) -> Hdr {
    let dv = video.iter().find_map(|v| v.dv);
    let base = video.first();
    let format = match (dv, base.and_then(|v| v.transfer)) {
        // Profile 5 has no fallback layer: its PQ is Dolby's own colour space, not HDR10.
        (Some(d), _) if d.profile == 5 => "dv",
        (_, Some(16)) => "hdr10",
        (_, Some(18)) => "hlg",
        (_, Some(2) | None) => "unknown",
        (_, Some(_)) => "sdr",
    };
    Hdr {
        format: format.into(),
        hdr10plus: base.and_then(|v| v.hdr10plus),
        dv,
    }
}

fn audio_score(a: &AudioTrack) -> (bool, u8, bool) {
    (
        a.lossless == Some(true),
        a.channels.unwrap_or(0),
        a.atmos == Some(true),
    )
}

// The first of the best-scoring tracks.
fn best_audio(audio: &[AudioTrack]) -> Option<usize> {
    let mut best: Option<usize> = None;
    for (i, a) in audio.iter().enumerate() {
        if best.is_none_or(|b| audio_score(a) > audio_score(&audio[b])) {
            best = Some(i);
        }
    }
    best
}

// What a better release of `tier` would carry, split into missing and unsettled.
fn radar(tier: Option<&str>, hdr: &Hdr, audio: &[AudioTrack]) -> (Vec<String>, Vec<String>) {
    let (mut missing, mut unknown) = (Vec::new(), Vec::new());
    let Some(tier) = tier else {
        return (missing, unknown);
    };
    let mut check = |name: &str, has: Option<bool>| match has {
        Some(true) => {}
        Some(false) => missing.push(name.to_string()),
        None => unknown.push(name.to_string()),
    };
    // Any track that has it settles it; else any track that might settles nothing.
    let any = |f: fn(&AudioTrack) -> Option<bool>| {
        if audio.iter().any(|a| f(a) == Some(true)) {
            Some(true)
        } else if audio.iter().any(|a| f(a).is_none()) {
            None
        } else {
            Some(false)
        }
    };
    check("lossless", any(|a| a.lossless));
    if tier == "uhd" {
        check("DV", Some(hdr.dv.is_some()));
        check("Atmos", any(|a| a.atmos));
        let most = audio.iter().filter_map(|a| a.channels).max();
        let all_known = audio.iter().all(|a| a.channels.is_some());
        check(
            "7.1",
            match most {
                Some(n) if n >= 8 => Some(true),
                _ if !all_known => None,
                _ => Some(false),
            },
        );
    } else {
        check("UHD", Some(false));
    }
    (missing, unknown)
}

fn timeline(declared: Option<f64>, last_cue: Option<f64>, last_frame: Option<f64>) -> Timeline {
    let delta = declared.zip(last_frame).map(|(d, f)| f - d);
    let state = match delta {
        None => "unknown",
        Some(x) if x > TIMELINE_TOLERANCE_SECS => "overrun",
        Some(x) if x < -TIMELINE_TOLERANCE_SECS => "short",
        Some(_) => "ok",
    };
    Timeline {
        declared_secs: declared,
        last_cue_secs: last_cue,
        last_frame_secs: last_frame,
        delta_secs: delta,
        state: state.into(),
    }
}

fn video_track(t: &Track, first: Option<&[u8]>) -> VideoTrack {
    let c = t.colour.unwrap_or_default();
    let hevc = t.codec_id.starts_with("V_MPEGH/ISO/HEVC");
    VideoTrack {
        number: t.number,
        codec: super::probe::codec_name(&t.codec_id),
        width: t.width.and_then(|w| u32::try_from(w).ok()),
        height: t.height.and_then(|h| u32::try_from(h).ok()),
        fps: t
            .default_duration
            .filter(|d| *d > 0)
            .map(|d| (1e9 / d as f64 * 1000.0).round() / 1000.0),
        bit_depth: c
            .bits
            .filter(|b| *b > 0)
            .map(|b| b as u8)
            .or_else(|| bit_depth(&t.codec_id, &t.codec_private)),
        interlaced: t.interlaced.and_then(|f| match f {
            1 => Some(true),
            2 => Some(false),
            _ => None,
        }),
        transfer: c.transfer,
        primaries: c.primaries,
        matrix: c.matrix,
        max_cll: c.max_cll,
        max_fall: c.max_fall,
        mastering_max_nits: c.mastering_max,
        mastering_min_nits: c.mastering_min,
        dv: t.dolby_vision(),
        hdr10plus: if hevc {
            first.and_then(|f| hevc_hdr10plus(f, hevc_nal_length(&t.codec_private)))
        } else {
            None
        },
        title: t.name.clone(),
        language: t.language(),
        default: t.default,
    }
}

// From the decoder configuration: HEVC's hvcC says it; AVC's 8-bit-only profiles imply it.
fn bit_depth(codec_id: &str, private: &[u8]) -> Option<u8> {
    if codec_id.starts_with("V_MPEGH/ISO/HEVC") && private.len() >= 23 && private[0] == 1 {
        return Some((private[17] & 7) + 8);
    }
    if codec_id.starts_with("V_MPEG4/ISO/AVC") && private.len() >= 2 && private[0] == 1 {
        return matches!(private[1], 66 | 77 | 88 | 100).then_some(8);
    }
    None
}

fn hevc_nal_length(private: &[u8]) -> usize {
    match private {
        [1, ..] if private.len() >= 23 => usize::from(private[21] & 3) + 1,
        _ => 4,
    }
}

// Claims a reader takes as "lossless", in a title or codec ID.
const LOSSLESS_CLAIMS: [&str; 7] = [
    "DTS-HD MA",
    "DTS-HD MASTER",
    "DTS/MA",
    "MASTER AUDIO",
    "TRUEHD",
    "MLP",
    "LOSSLESS",
];

fn audio_track(t: &Track, first: Option<&[u8]>) -> AudioTrack {
    let id = t.codec_id.as_str();
    let (format, lossless, atmos): (String, Option<bool>, Option<bool>) =
        if id.starts_with("A_TRUEHD") || id.starts_with("A_MLP") {
            ("TrueHD".into(), Some(true), first.and_then(truehd_atmos))
        } else if id.starts_with("A_DTS") {
            match first.and_then(dts_kind) {
                Some(k) => (k.name().into(), Some(k == DtsKind::HdMa), Some(false)),
                None => ("DTS".into(), None, Some(false)),
            }
        } else if id.starts_with("A_EAC3") {
            ("E-AC-3".into(), Some(false), first.and_then(eac3_atmos))
        } else if id.starts_with("A_PCM") || id.starts_with("A_FLAC") || id.starts_with("A_ALAC") {
            (super::probe::codec_name(id), Some(true), Some(false))
        } else if ["A_AC3", "A_AAC", "A_OPUS", "A_VORBIS", "A_MPEG/"]
            .iter()
            .any(|p| id.starts_with(p))
        {
            (super::probe::codec_name(id), Some(false), Some(false))
        } else {
            (super::probe::codec_name(id), None, None)
        };
    let claim = format!("{} {}", t.codec_id, t.name).to_ascii_uppercase();
    let claims = LOSSLESS_CLAIMS.iter().any(|c| claim.contains(c));
    AudioTrack {
        number: t.number,
        format,
        channels: t.channels.and_then(|c| u8::try_from(c).ok()),
        sample_rate: t.sample_rate.map(|r| r.round() as u32).filter(|r| *r > 0),
        bit_depth: t.bit_depth.and_then(|b| u8::try_from(b).ok()),
        lossless,
        atmos,
        language: t.language(),
        title: t.name.clone(),
        default: t.default,
        forced: t.forced,
        lossy_claim: claims && lossless == Some(false),
    }
}

// ── The bitstream looks ────────────────────────────────────────────────────

/// TrueHD carries Atmos as a fourth substream, counted in the major sync.
fn truehd_atmos(data: &[u8]) -> Option<bool> {
    let at = find(data, &[0xF8, 0x72, 0x6F, 0xBA])?;
    data.get(at + 16).map(|b| (b >> 4) >= 4)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DtsKind {
    Core,
    HdMa,
    HdHra,
    Express,
}

impl DtsKind {
    fn name(self) -> &'static str {
        match self {
            Self::Core => "DTS",
            Self::HdMa => "DTS-HD MA",
            Self::HdHra => "DTS-HD HRA",
            Self::Express => "DTS Express",
        }
    }
}

/// DTS-HD rides in an extension substream after the core: XLL there is Master Audio
/// (lossless), LBR is Express; any other extension is High Resolution (lossy).
fn dts_kind(data: &[u8]) -> Option<DtsKind> {
    const CORE: [u8; 4] = [0x7F, 0xFE, 0x80, 0x01];
    const EXSS: [u8; 4] = [0x64, 0x58, 0x20, 0x25];
    const XLL: [u8; 4] = [0x41, 0xA2, 0x95, 0x47];
    const LBR: [u8; 4] = [0x0A, 0x80, 0x19, 0x21];
    match find(data, &EXSS) {
        Some(at) if find(&data[at..], &XLL).is_some() => Some(DtsKind::HdMa),
        Some(at) if find(&data[at..], &LBR).is_some() => Some(DtsKind::Express),
        Some(_) => Some(DtsKind::HdHra),
        None => find(data, &CORE).map(|_| DtsKind::Core),
    }
}

/// E-AC-3 signals Atmos (JOC) with `flag_ec3_extension_type_a`, the low bit of the first
/// additional-bsi byte of an independent frame (ETSI TS 102 366, as FFmpeg reads it).
fn eac3_atmos(data: &[u8]) -> Option<bool> {
    let mut from = 0;
    for _ in 0..16 {
        let at = from + find(&data[from..], &[0x0B, 0x77])?;
        if let Some(v) = eac3_frame_atmos(&data[at + 2..]) {
            return Some(v);
        }
        from = at + 2;
    }
    None
}

// `None` for anything but a whole independent E-AC-3 bsi.
fn eac3_frame_atmos(b: &[u8]) -> Option<bool> {
    let mut r = Bits { b, pos: 0 };
    let strmtyp = r.get(2)?;
    r.skip(3 + 11)?;
    let fscod = r.get(2)?;
    let blocks = if fscod == 3 {
        r.skip(2)?;
        6
    } else {
        [1, 2, 3, 6][r.get(2)? as usize]
    };
    let acmod = r.get(3)?;
    let lfeon = r.get(1)? == 1;
    let bsid = r.get(5)?;
    if strmtyp != 0 || !(11..=16).contains(&bsid) {
        return None;
    }
    let programs = if acmod == 0 { 2 } else { 1 };
    for _ in 0..programs {
        r.skip(5)?;
        if r.flag()? {
            r.skip(8)?;
        }
    }
    if r.flag()? {
        if acmod > 2 {
            r.skip(2)?;
            if acmod & 1 == 1 {
                r.skip(6)?;
            }
            if acmod & 4 != 0 {
                r.skip(6)?;
            }
        }
        if lfeon && r.flag()? {
            r.skip(5)?;
        }
        for _ in 0..programs {
            if r.flag()? {
                r.skip(6)?;
            }
        }
        if r.flag()? {
            r.skip(6)?;
        }
        match r.get(2)? {
            1 => r.skip(5)?,
            2 => r.skip(12)?,
            3 => {
                let n = (r.get(5)? as usize + 2) * 8;
                r.skip(n)?;
            }
            _ => {}
        }
        if acmod < 2 {
            for _ in 0..programs {
                if r.flag()? {
                    r.skip(14)?;
                }
            }
        }
        if r.flag()? {
            for _ in 0..blocks {
                if blocks == 1 || r.flag()? {
                    r.skip(5)?;
                }
            }
        }
    }
    if r.flag()? {
        r.skip(5)?;
        if acmod == 2 {
            r.skip(4)?;
        }
        if acmod >= 6 {
            r.skip(2)?;
        }
        for _ in 0..programs {
            if r.flag()? {
                r.skip(8)?;
            }
        }
        if fscod != 3 {
            r.skip(1)?;
        }
    }
    if blocks != 6 {
        r.skip(1)?;
    }
    if !r.flag()? {
        return Some(false);
    }
    r.skip(6 + 7)?;
    r.flag()
}

/// HDR10+ is an ITU-T T.35 SEI (Samsung, application 4) ahead of the first slice.
/// `None` when the read data ends before a slice does.
fn hevc_hdr10plus(frame: &[u8], len_size: usize) -> Option<bool> {
    const HDR10PLUS: [u8; 6] = [0xB5, 0x00, 0x3C, 0x00, 0x01, 0x04];
    let mut p = 0;
    while p + len_size < frame.len() {
        let len = frame[p..p + len_size]
            .iter()
            .fold(0usize, |a, &x| (a << 8) | usize::from(x));
        let start = p + len_size;
        let nal = &frame[start..frame.len().min(start.saturating_add(len))];
        let kind = nal.first().map(|h| (h >> 1) & 0x3F)?;
        if kind <= 31 {
            return Some(false);
        }
        if kind == 39 && nal.len() > 2 {
            let rbsp = unescape(&nal[2..]);
            let mut q = 0;
            while q < rbsp.len() && rbsp[q] != 0x80 {
                let (ptype, n) = sei_number(&rbsp[q..])?;
                q += n;
                let (size, n) = sei_number(&rbsp[q..])?;
                q += n;
                let payload = &rbsp[q.min(rbsp.len())..rbsp.len().min(q + size)];
                if ptype == 4 && payload.starts_with(&HDR10PLUS) {
                    return Some(true);
                }
                q += size;
            }
        }
        p = start.saturating_add(len);
    }
    None
}

// An SEI payload type or size: 0xFF bytes add 255 each, then the last byte.
fn sei_number(b: &[u8]) -> Option<(usize, usize)> {
    let mut v = 0;
    for (i, &x) in b.iter().enumerate() {
        v += usize::from(x);
        if x != 0xFF {
            return Some((v, i + 1));
        }
    }
    None
}

// NAL payload without its emulation-prevention bytes (00 00 03 → 00 00).
fn unescape(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len());
    let mut zeros = 0;
    for &x in b {
        if zeros >= 2 && x == 3 {
            zeros = 0;
            continue;
        }
        zeros = if x == 0 { zeros + 1 } else { 0 };
        out.push(x);
    }
    out
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

struct Bits<'a> {
    b: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn get(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *self.b.get(self.pos / 8)?;
            v = (v << 1) | u32::from((byte >> (7 - self.pos % 8)) & 1);
            self.pos += 1;
        }
        Some(v)
    }

    fn flag(&mut self) -> Option<bool> {
        self.get(1).map(|v| v == 1)
    }

    fn skip(&mut self, n: usize) -> Option<()> {
        self.pos += n;
        (self.pos <= self.b.len() * 8).then_some(())
    }
}

// ── Matroska ───────────────────────────────────────────────────────────────

const EBML: u32 = 0x1A45_DFA3;
const SEGMENT: u32 = 0x1853_8067;
const SEEK_HEAD: u32 = 0x114D_9B74;
const SEEK: u32 = 0x4DBB;
const SEEK_ID: u32 = 0x53AB;
const SEEK_POSITION: u32 = 0x53AC;
const INFO: u32 = 0x1549_A966;
const TRACKS: u32 = 0x1654_AE6B;
const TRACK_ENTRY: u32 = 0xAE;
const CUES: u32 = 0x1C53_BB6B;
const CUE_POINT: u32 = 0xBB;
const CUE_TIME: u32 = 0xB3;
const CUE_TRACK_POSITIONS: u32 = 0xB7;
const CUE_CLUSTER_POSITION: u32 = 0xF1;
const CLUSTER: u32 = 0x1F43_B675;
const CLUSTER_TIMESTAMP: u32 = 0xE7;
const SIMPLE_BLOCK: u32 = 0xA3;
const BLOCK_GROUP: u32 = 0xA0;
const BLOCK: u32 = 0xA1;

// Element ID (marker bits kept) and its length.
fn vint_id(b: &[u8]) -> Option<(u32, usize)> {
    let len = b.first()?.leading_zeros() as usize + 1;
    if len > 4 || b.len() < len {
        return None;
    }
    Some((
        b[..len].iter().fold(0, |a, &x| (a << 8) | u32::from(x)),
        len,
    ))
}

// Element size and its length; `u64::MAX` for "unknown".
fn vint_size(b: &[u8]) -> Option<(u64, usize)> {
    let len = b.first()?.leading_zeros() as usize + 1;
    if len > 8 || b.len() < len {
        return None;
    }
    let mask = (0xFFu16 >> len) as u8;
    let mut v = u64::from(b[0] & mask);
    let mut ones = b[0] & mask == mask;
    for &x in &b[1..len] {
        v = (v << 8) | u64::from(x);
        ones &= x == 0xFF;
    }
    Some((if ones { u64::MAX } else { v }, len))
}

// The children of a master element's body; stops at the first malformed one.
fn children(mut b: &[u8]) -> impl Iterator<Item = (u32, &[u8])> {
    std::iter::from_fn(move || {
        let (id, il) = vint_id(b)?;
        let (size, sl) = vint_size(&b[il..])?;
        let start = il + sl;
        let end = start.checked_add(usize::try_from(size).ok()?)?;
        let body = b.get(start..end)?;
        b = &b[end..];
        Some((id, body))
    })
}

fn uint(b: &[u8]) -> u64 {
    b.iter().take(8).fold(0, |a, &x| (a << 8) | u64::from(x))
}

fn float(b: &[u8]) -> Option<f64> {
    match b.len() {
        0 => Some(0.0),
        4 => Some(f64::from(f32::from_be_bytes(b.try_into().ok()?))),
        8 => Some(f64::from_be_bytes(b.try_into().ok()?)),
        _ => None,
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b)
        .trim_end_matches('\0')
        .to_string()
}

fn invalid() -> io::Error {
    io::Error::from(io::ErrorKind::InvalidData)
}

// One element header from a stream: id, size, header length.
fn read_head(r: &mut impl Read) -> io::Result<(u32, u64, u64)> {
    let mut b = [0u8; 12];
    r.read_exact(&mut b[..1])?;
    let il = b[0].leading_zeros() as usize + 1;
    if il > 4 {
        return Err(invalid());
    }
    r.read_exact(&mut b[1..il])?;
    let (id, _) = vint_id(&b[..il]).ok_or_else(invalid)?;
    r.read_exact(&mut b[il..il + 1])?;
    let sl = b[il].leading_zeros() as usize + 1;
    if sl > 8 {
        return Err(invalid());
    }
    r.read_exact(&mut b[il + 1..il + sl])?;
    let (size, _) = vint_size(&b[il..il + sl]).ok_or_else(invalid)?;
    Ok((id, size, (il + sl) as u64))
}

fn read_body(r: &mut impl Read, size: u64, cap: u64) -> io::Result<Option<Vec<u8>>> {
    if size > cap {
        return Ok(None);
    }
    let mut b = vec![0u8; size as usize];
    r.read_exact(&mut b)?;
    Ok(Some(b))
}

fn goto<R: Read + Seek>(r: &mut BufReader<R>, pos: u64) -> io::Result<()> {
    let here = r.stream_position()?;
    match i64::try_from(pos.wrapping_sub(here)) {
        Ok(d) if pos >= here => r.seek_relative(d),
        _ => r.seek(io::SeekFrom::Start(pos)).map(|_| ()),
    }
}

#[derive(Default)]
struct Layout {
    seg_start: u64,
    seg_end: u64,
    info: Option<Vec<u8>>,
    tracks: Option<Vec<u8>>,
    cues: Option<Vec<u8>>,
    first_cluster: Option<u64>,
}

// The Segment's level-1 elements up to the first cluster, then whatever the SeekHead points
// past it (Cues usually sit at the end).
fn layout<R: Read + Seek>(r: &mut BufReader<R>) -> io::Result<Layout> {
    goto(r, 0)?;
    let (id, size, hl) = read_head(r)?;
    if id != EBML || size == u64::MAX {
        return Err(invalid());
    }
    let header_end = hl + size;
    goto(r, header_end)?;
    let (id, size, hl) = read_head(r)?;
    if id != SEGMENT {
        return Err(invalid());
    }
    let seg_start = header_end + hl;
    let mut lay = Layout {
        seg_start,
        seg_end: seg_start.saturating_add(size),
        ..Layout::default()
    };
    let mut seeks: HashMap<u32, u64> = HashMap::new();
    let mut pos = seg_start;
    while pos < lay.seg_end {
        goto(r, pos)?;
        let (id, size, hl) = match read_head(r) {
            Ok(h) => h,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e),
        };
        match id {
            INFO => lay.info = read_body(r, size, 1 << 20)?,
            TRACKS => lay.tracks = read_body(r, size, 16 << 20)?,
            CUES => lay.cues = read_body(r, size, 64 << 20)?,
            SEEK_HEAD => {
                let body = read_body(r, size, 1 << 20)?.unwrap_or_default();
                for (_, seek) in children(&body).filter(|(i, _)| *i == SEEK) {
                    let mut target = (None, None);
                    for (cid, cb) in children(seek) {
                        match cid {
                            SEEK_ID => target.0 = Some(uint(cb) as u32),
                            SEEK_POSITION => target.1 = Some(uint(cb)),
                            _ => {}
                        }
                    }
                    if let (Some(id), Some(at)) = target {
                        seeks.entry(id).or_insert(at);
                    }
                }
            }
            CLUSTER => {
                lay.first_cluster = Some(pos);
                break;
            }
            _ => {}
        }
        if size == u64::MAX {
            break;
        }
        pos = pos.saturating_add(hl).saturating_add(size);
    }
    for (id, slot, cap) in [
        (INFO, &mut lay.info, 1u64 << 20),
        (TRACKS, &mut lay.tracks, 16 << 20),
        (CUES, &mut lay.cues, 64 << 20),
    ] {
        if slot.is_some() {
            continue;
        }
        let Some(rel) = seeks.get(&id) else { continue };
        goto(r, seg_start.saturating_add(*rel))?;
        let (got, size, _) = read_head(r)?;
        if got == id {
            *slot = read_body(r, size, cap)?;
        }
    }
    Ok(lay)
}

// The latest CuePoint: its time and its cluster's position in the Segment.
fn last_cue(cues: &[u8]) -> Option<(u64, u64)> {
    let mut last: Option<(u64, u64)> = None;
    for (_, point) in children(cues).filter(|(id, _)| *id == CUE_POINT) {
        let mut time = None;
        let mut at = None;
        for (id, b) in children(point) {
            match id {
                CUE_TIME => time = Some(uint(b)),
                CUE_TRACK_POSITIONS if at.is_none() => {
                    at = children(b)
                        .find(|(i, _)| *i == CUE_CLUSTER_POSITION)
                        .map(|(_, p)| uint(p));
                }
                _ => {}
            }
        }
        if let (Some(t), Some(p)) = (time, at)
            && last.is_none_or(|(lt, _)| t >= lt)
        {
            last = Some((t, p));
        }
    }
    last
}

// One step of a flat walk over clusters: a cluster or block group is entered, a block is
// handed to `on_block` (track, relative timestamp, body length), anything else is skipped.
fn walk_blocks<R: Read + Seek>(
    r: &mut BufReader<R>,
    from: u64,
    end: u64,
    mut on: impl FnMut(&mut BufReader<R>, Step) -> io::Result<bool>,
) -> io::Result<()> {
    let mut pos = from;
    let mut blocks = 0u32;
    while pos < end && blocks < MAX_BLOCKS {
        goto(r, pos)?;
        let (id, size, hl) = match read_head(r) {
            Ok(h) => h,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        };
        match id {
            CLUSTER | BLOCK_GROUP => {
                if id == CLUSTER && !on(r, Step::Cluster)? {
                    return Ok(());
                }
                pos += hl;
                continue;
            }
            CLUSTER_TIMESTAMP if size <= 8 => {
                let mut b = [0u8; 8];
                r.read_exact(&mut b[..size as usize])?;
                if !on(r, Step::Timestamp(uint(&b[..size as usize])))? {
                    return Ok(());
                }
            }
            SIMPLE_BLOCK | BLOCK if size != u64::MAX => {
                blocks += 1;
                let mut h = [0u8; 12];
                let n = size.min(12) as usize;
                r.read_exact(&mut h[..n])?;
                let (track, tl) = vint_size(&h[..n]).ok_or_else(invalid)?;
                let rel = h
                    .get(tl..tl + 2)
                    .filter(|_| tl + 3 <= n)
                    .map(|b| i16::from_be_bytes([b[0], b[1]]))
                    .ok_or_else(invalid)?;
                goto(r, pos + hl + tl as u64 + 3)?;
                let step = Step::Block {
                    track,
                    rel,
                    len: size - tl as u64 - 3,
                };
                if !on(r, step)? {
                    return Ok(());
                }
            }
            _ => {}
        }
        if size == u64::MAX {
            return Ok(());
        }
        pos = pos.saturating_add(hl).saturating_add(size);
    }
    Ok(())
}

enum Step {
    Cluster,
    Timestamp(u64),
    Block { track: u64, rel: i16, len: u64 },
}

// The first bytes of each track the bitstream looks need, from the first clusters.
fn sniff<R: Read + Seek>(
    r: &mut BufReader<R>,
    first_cluster: u64,
    seg_end: u64,
    tracks: &[Track],
) -> io::Result<HashMap<u64, Vec<u8>>> {
    struct Want {
        bytes: usize,
        blocks: u32,
        strip: Vec<u8>,
    }
    let mut wants: HashMap<u64, Want> = tracks
        .iter()
        .filter(|t| !t.opaque)
        .filter_map(|t| {
            let id = t.codec_id.as_str();
            let (bytes, blocks) = if t.kind == 1 && id.starts_with("V_MPEGH/ISO/HEVC") {
                (SNIFF_VIDEO_BYTES, 1)
            } else if t.kind == 2
                && ["A_TRUEHD", "A_MLP", "A_DTS", "A_EAC3"]
                    .iter()
                    .any(|p| id.starts_with(p))
            {
                (SNIFF_AUDIO_BYTES, SNIFF_AUDIO_BLOCKS)
            } else {
                return None;
            };
            let strip = t.strip.clone().unwrap_or_default();
            Some((
                t.number,
                Want {
                    bytes,
                    blocks,
                    strip,
                },
            ))
        })
        .collect();
    let mut got: HashMap<u64, Vec<u8>> = HashMap::new();
    if wants.is_empty() {
        return Ok(got);
    }
    let end = seg_end.min(first_cluster.saturating_add(SNIFF_SPAN));
    walk_blocks(r, first_cluster, end, |r, step| {
        let Step::Block { track, len, .. } = step else {
            return Ok(true);
        };
        let Some(w) = wants.get_mut(&track) else {
            return Ok(true);
        };
        let buf = got.entry(track).or_default();
        let room = w.bytes.saturating_sub(buf.len());
        buf.extend_from_slice(&w.strip);
        let take = (len as usize).min(room);
        let at = buf.len();
        buf.resize(at + take, 0);
        r.read_exact(&mut buf[at..])?;
        w.blocks -= 1;
        if w.blocks == 0 || buf.len() >= w.bytes {
            wants.remove(&track);
        }
        Ok(!wants.is_empty())
    })?;
    Ok(got)
}

// The latest block timestamp (absolute ticks) from `from` to the end of the Segment.
fn last_block_ticks<R: Read + Seek>(
    r: &mut BufReader<R>,
    from: u64,
    seg_end: u64,
) -> io::Result<Option<u64>> {
    let mut cluster: Option<u64> = None;
    let mut last: Option<u64> = None;
    let end = seg_end.min(from.saturating_add(TAIL_SPAN));
    walk_blocks(r, from, end, |_, step| {
        match step {
            Step::Cluster => cluster = None,
            Step::Timestamp(t) => cluster = Some(t),
            Step::Block { rel, .. } => {
                if let Some(c) = cluster {
                    let t = c.saturating_add_signed(i64::from(rel));
                    last = Some(last.map_or(t, |l| l.max(t)));
                }
            }
        }
        Ok(true)
    })?;
    Ok(last)
}

#[derive(Default)]
struct Info {
    scale: u64,
    duration: Option<f64>,
    raw: Vec<(String, String)>,
}

impl Info {
    fn parse(b: &[u8]) -> Self {
        let mut i = Info {
            scale: 1_000_000,
            ..Info::default()
        };
        for (id, body) in children(b) {
            let (name, value) = match id {
                0x2A_D7B1 => {
                    i.scale = uint(body).max(1);
                    ("TimestampScale", format!("{} ns", i.scale))
                }
                0x4489 => {
                    i.duration = float(body);
                    ("Duration", format!("{} ticks", i.duration.unwrap_or(0.0)))
                }
                0x7BA9 => ("Title", text(body)),
                0x4D80 => ("MuxingApp", text(body)),
                0x5741 => ("WritingApp", text(body)),
                0x4461 => ("DateUTC", date(body)),
                0x73A4 => ("SegmentUUID", hex(body)),
                _ => continue,
            };
            i.raw.push((name.into(), value));
        }
        i
    }
}

fn date(b: &[u8]) -> String {
    // Nanoseconds since 2001-01-01T00:00:00 UTC.
    let ns = i64::from_be_bytes(b.try_into().unwrap_or([0; 8]));
    let secs = 978_307_200 + ns.div_euclid(1_000_000_000);
    crate::server::util::iso_datetime_at(u64::try_from(secs).unwrap_or(0))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Colour {
    matrix: Option<u8>,
    transfer: Option<u8>,
    primaries: Option<u8>,
    bits: Option<u64>,
    max_cll: Option<u32>,
    max_fall: Option<u32>,
    mastering_max: Option<f64>,
    mastering_min: Option<f64>,
}

#[derive(Clone, Debug, Default)]
struct Mapping {
    kind: u64,
    extra: Vec<u8>,
}

#[derive(Clone, Debug)]
struct Track {
    number: u64,
    kind: u64,
    codec_id: String,
    codec_private: Vec<u8>,
    name: String,
    lang: Option<String>,
    lang_bcp47: Option<String>,
    default: bool,
    forced: bool,
    default_duration: Option<u64>,
    width: Option<u64>,
    height: Option<u64>,
    interlaced: Option<u64>,
    colour: Option<Colour>,
    sample_rate: Option<f64>,
    channels: Option<u64>,
    bit_depth: Option<u64>,
    mappings: Vec<Mapping>,
    /// Header-stripping bytes to put back in front of each frame.
    strip: Option<Vec<u8>>,
    /// Compressed or encrypted frames: nothing to look at.
    opaque: bool,
    raw: Vec<(String, String)>,
}

impl Default for Track {
    fn default() -> Self {
        Self {
            number: 0,
            kind: 0,
            codec_id: String::new(),
            codec_private: Vec::new(),
            name: String::new(),
            lang: None,
            lang_bcp47: None,
            // Matroska's defaults: a track is a default track unless it says otherwise.
            default: true,
            forced: false,
            default_duration: None,
            width: None,
            height: None,
            interlaced: None,
            colour: None,
            sample_rate: None,
            channels: None,
            bit_depth: None,
            mappings: Vec::new(),
            strip: None,
            opaque: false,
            raw: Vec::new(),
        }
    }
}

impl Track {
    // Matroska's default language is English.
    fn language(&self) -> String {
        self.lang_bcp47
            .clone()
            .or_else(|| self.lang.clone())
            .unwrap_or_else(|| "eng".into())
    }

    fn dolby_vision(&self) -> Option<DolbyVision> {
        const DVCC: u64 = u32::from_be_bytes(*b"dvcC") as u64;
        const DVVC: u64 = u32::from_be_bytes(*b"dvvC") as u64;
        const DVWC: u64 = u32::from_be_bytes(*b"dvwC") as u64;
        let m = self
            .mappings
            .iter()
            .find(|m| [DVCC, DVVC, DVWC].contains(&m.kind) && m.extra.len() >= 5)?;
        let e = &m.extra;
        Some(DolbyVision {
            profile: e[2] >> 1,
            level: ((e[2] & 1) << 5) | (e[3] >> 3),
            rpu: e[3] & 4 != 0,
            el: e[3] & 2 != 0,
            bl: e[3] & 1 != 0,
            compat_id: e[4] >> 4,
        })
    }

    fn parse(b: &[u8]) -> Self {
        let mut t = Track::default();
        let mut raw = Vec::new();
        t.walk(b, "", &mut raw);
        t.raw = raw;
        t
    }

    // Read one master body, recording every field (nested ones under their parent's name).
    fn walk(&mut self, b: &[u8], parent: &str, raw: &mut Vec<(String, String)>) {
        for (id, body) in children(b) {
            let u = || uint(body);
            let f = || float(body).unwrap_or(0.0);
            let (name, value): (&str, String) = match id {
                0xD7 => {
                    self.number = u();
                    ("TrackNumber", u().to_string())
                }
                0x73C5 => ("TrackUID", u().to_string()),
                0x83 => {
                    self.kind = u();
                    ("TrackType", format!("{} ({})", u(), kind_name(u())))
                }
                0xB9 => ("FlagEnabled", u().to_string()),
                0x88 => {
                    self.default = u() != 0;
                    ("FlagDefault", u().to_string())
                }
                0x55AA => {
                    self.forced = u() != 0;
                    ("FlagForced", u().to_string())
                }
                0x55AB => ("FlagHearingImpaired", u().to_string()),
                0x55AC => ("FlagVisualImpaired", u().to_string()),
                0x55AD => ("FlagTextDescriptions", u().to_string()),
                0x55AE => ("FlagOriginal", u().to_string()),
                0x55AF => ("FlagCommentary", u().to_string()),
                0x9C => ("FlagLacing", u().to_string()),
                0x23_E383 => {
                    self.default_duration = Some(u());
                    ("DefaultDuration", format!("{} ns", u()))
                }
                0x536E => {
                    self.name = text(body);
                    ("Name", text(body))
                }
                0x22_B59C => {
                    self.lang = Some(text(body));
                    ("Language", text(body))
                }
                0x22_B59D => {
                    self.lang_bcp47 = Some(text(body));
                    ("LanguageBCP47", text(body))
                }
                0x86 => {
                    self.codec_id = text(body);
                    ("CodecID", text(body))
                }
                0x63A2 => {
                    self.codec_private = body.to_vec();
                    ("CodecPrivate", format!("{} bytes", body.len()))
                }
                0x25_8688 => ("CodecName", text(body)),
                0x56AA => ("CodecDelay", format!("{} ns", u())),
                0x56BB => ("SeekPreRoll", format!("{} ns", u())),
                0x55EE => ("MaxBlockAdditionID", u().to_string()),
                0xB0 => {
                    self.width = Some(u());
                    ("PixelWidth", u().to_string())
                }
                0xBA => {
                    self.height = Some(u());
                    ("PixelHeight", u().to_string())
                }
                0x54B0 => ("DisplayWidth", u().to_string()),
                0x54BA => ("DisplayHeight", u().to_string()),
                0x54B2 => ("DisplayUnit", u().to_string()),
                0x9A => {
                    self.interlaced = Some(u());
                    ("FlagInterlaced", u().to_string())
                }
                0x9D => ("FieldOrder", u().to_string()),
                0x53B8 => ("StereoMode", u().to_string()),
                0x2383E3 => ("FrameRate", f().to_string()),
                0xB5 => {
                    self.sample_rate = Some(f());
                    ("SamplingFrequency", format!("{} Hz", f()))
                }
                0x78B5 => ("OutputSamplingFrequency", format!("{} Hz", f())),
                0x9F => {
                    self.channels = Some(u());
                    ("Channels", u().to_string())
                }
                0x6264 => {
                    self.bit_depth = Some(u());
                    ("BitDepth", u().to_string())
                }
                0x55B1 => {
                    self.colour_mut().matrix = u8::try_from(u()).ok();
                    ("MatrixCoefficients", u().to_string())
                }
                0x55B2 => {
                    self.colour_mut().bits = Some(u());
                    ("BitsPerChannel", u().to_string())
                }
                0x55B9 => ("Range", u().to_string()),
                0x55BA => {
                    self.colour_mut().transfer = u8::try_from(u()).ok();
                    ("TransferCharacteristics", u().to_string())
                }
                0x55BB => {
                    self.colour_mut().primaries = u8::try_from(u()).ok();
                    ("Primaries", u().to_string())
                }
                0x55BC => {
                    self.colour_mut().max_cll = u32::try_from(u()).ok();
                    ("MaxCLL", format!("{} cd/m²", u()))
                }
                0x55BD => {
                    self.colour_mut().max_fall = u32::try_from(u()).ok();
                    ("MaxFALL", format!("{} cd/m²", u()))
                }
                0x55D9 => {
                    self.colour_mut().mastering_max = Some(f());
                    ("LuminanceMax", format!("{} cd/m²", f()))
                }
                0x55DA => {
                    self.colour_mut().mastering_min = Some(f());
                    ("LuminanceMin", format!("{} cd/m²", f()))
                }
                0x55D1..=0x55D8 => ("Chromaticity", format!("{:.4}", f())),
                0x41E7 => (
                    "BlockAddIDType",
                    match u32::try_from(u()).map(u32::to_be_bytes) {
                        Ok(cc) if cc.iter().all(u8::is_ascii_alphanumeric) => {
                            String::from_utf8_lossy(&cc).into_owned()
                        }
                        _ => u().to_string(),
                    },
                ),
                0x41ED => ("BlockAddIDExtraData", hex(body)),
                0x41F0 => ("BlockAddIDValue", u().to_string()),
                0x41A4 => ("BlockAddIDName", text(body)),
                0x4254 => ("ContentCompAlgo", u().to_string()),
                0x4255 => ("ContentCompSettings", hex(body)),
                // Masters: recorded by name, then their children under it.
                0xE0 | 0xE1 | 0x55B0 | 0x55D0 | 0x6D80 | 0x6240 | 0x5034 | 0x5035 => {
                    let name = match id {
                        0xE0 => "Video",
                        0xE1 => "Audio",
                        0x55B0 => "Colour",
                        0x55D0 => "MasteringMetadata",
                        0x6D80 => "ContentEncodings",
                        0x6240 => "ContentEncoding",
                        0x5034 => "ContentCompression",
                        _ => "ContentEncryption",
                    };
                    if id == 0x55B0 {
                        self.colour_mut();
                    }
                    if id == 0x5035 {
                        self.opaque = true;
                    }
                    if id == 0x5034 {
                        self.compression(body);
                    }
                    let path = join(parent, name);
                    self.walk(body, &path, raw);
                    continue;
                }
                0x41E4 => {
                    let mut m = Mapping::default();
                    for (cid, cb) in children(body) {
                        match cid {
                            0x41E7 => m.kind = uint(cb),
                            0x41ED => m.extra = cb.to_vec(),
                            _ => {}
                        }
                    }
                    self.mappings.push(m);
                    let path = join(parent, "BlockAdditionMapping");
                    self.walk(body, &path, raw);
                    continue;
                }
                _ => {
                    raw.push((
                        join(parent, &format!("0x{id:X}")),
                        format!("{} bytes", body.len()),
                    ));
                    continue;
                }
            };
            raw.push((join(parent, name), value));
        }
    }

    fn colour_mut(&mut self) -> &mut Colour {
        self.colour.get_or_insert_with(Colour::default)
    }

    // Header stripping (algorithm 3) can be undone for the bitstream looks; zlib and the
    // others cannot, cheaply.
    fn compression(&mut self, body: &[u8]) {
        let mut algo = 0;
        let mut settings = Vec::new();
        for (id, b) in children(body) {
            match id {
                0x4254 => algo = uint(b),
                0x4255 => settings = b.to_vec(),
                _ => {}
            }
        }
        if algo == 3 {
            self.strip = Some(settings);
        } else {
            self.opaque = true;
        }
    }
}

fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent} › {name}")
    }
}

#[cfg(test)]
#[path = "media_tests.rs"]
pub(crate) mod tests;
