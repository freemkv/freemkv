use super::*;

pub(crate) fn el(id: u32, body: &[u8]) -> Vec<u8> {
    let mut out: Vec<u8> = id
        .to_be_bytes()
        .into_iter()
        .skip_while(|b| *b == 0)
        .collect();
    out.push(0x01);
    out.extend_from_slice(&(body.len() as u64).to_be_bytes()[1..]);
    out.extend_from_slice(body);
    out
}

pub(crate) fn uint_el(id: u32, v: u64) -> Vec<u8> {
    el(id, &v.to_be_bytes())
}

/// A TrackEntry: number, type (1 video, 2 audio, 17 subtitles), codec, more children.
pub(crate) fn entry(n: u64, kind: u64, codec: &str, more: &[Vec<u8>]) -> Vec<u8> {
    let mut b = uint_el(0xD7, n);
    b.extend(uint_el(0x83, kind));
    b.extend(el(0x86, codec.as_bytes()));
    for m in more {
        b.extend_from_slice(m);
    }
    b
}

pub(crate) fn video(w: u64, h: u64, colour: &[Vec<u8>]) -> Vec<u8> {
    let mut b = uint_el(0xB0, w);
    b.extend(uint_el(0xBA, h));
    if !colour.is_empty() {
        b.extend(el(0x55B0, &colour.concat()));
    }
    el(0xE0, &b)
}

pub(crate) fn audio(channels: u64) -> Vec<u8> {
    let mut b = el(0xB5, &48000f64.to_be_bytes());
    b.extend(uint_el(0x9F, channels));
    el(0xE1, &b)
}

pub(crate) fn dv(profile: u8, compat: u8) -> Vec<u8> {
    let level = 6u8;
    let rec = [
        1,
        0,
        (profile << 1) | (level >> 5),
        ((level & 0x1F) << 3) | 0b101,
        compat << 4,
    ];
    let mut b = uint_el(0x41E7, u64::from(u32::from_be_bytes(*b"dvcC")));
    b.extend(el(0x41ED, &rec));
    el(0x41E4, &b)
}

/// An MKV: Info, Tracks, one cluster of `(track, ms, data)` blocks, Cues to it.
pub(crate) fn mkv(
    entries: &[Vec<u8>],
    blocks: &[(u64, i16, Vec<u8>)],
    duration_ms: f64,
) -> Vec<u8> {
    let mut info = uint_el(0x2A_D7B1, 1_000_000);
    info.extend(el(0x4489, &duration_ms.to_be_bytes()));
    info.extend(el(0x4D80, b"freemkv 1.7.7"));
    info.extend(el(0x5741, b"freemkv 1.7.7"));
    let tracks: Vec<u8> = entries.iter().flat_map(|e| el(TRACK_ENTRY, e)).collect();
    // The SeekHead points at the Cues past the cluster; its size does not depend on where.
    let seek_head = |cues_at: u64| {
        let mut seek = el(SEEK_ID, &CUES.to_be_bytes());
        seek.extend(uint_el(SEEK_POSITION, cues_at));
        el(SEEK_HEAD, &el(SEEK, &seek))
    };
    let mut seg = seek_head(0);
    seg.extend(el(INFO, &info));
    seg.extend(el(TRACKS, &tracks));
    let at = seg.len() as u64;
    let mut cluster = uint_el(CLUSTER_TIMESTAMP, 0);
    for (track, ms, data) in blocks {
        let mut b = vec![0x80 | *track as u8];
        b.extend(ms.to_be_bytes());
        b.push(0x80);
        b.extend(data);
        cluster.extend(el(SIMPLE_BLOCK, &b));
    }
    seg.extend(el(CLUSTER, &cluster));
    let cues_at = seg.len() as u64;
    let head = seek_head(cues_at);
    seg[..head.len()].copy_from_slice(&head);
    let mut pos = uint_el(0xF7, 1);
    pos.extend(uint_el(CUE_CLUSTER_POSITION, at));
    let mut point = uint_el(CUE_TIME, 0);
    point.extend(el(CUE_TRACK_POSITIONS, &pos));
    seg.extend(el(CUES, &el(CUE_POINT, &point)));
    let mut out = el(EBML, &el(0x4282, b"matroska"));
    out.extend(el(SEGMENT, &seg));
    out
}

fn detail(bytes: Vec<u8>) -> MediaDetail {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("a.mkv");
    std::fs::write(&p, bytes).unwrap();
    read(&p).unwrap()
}

// An hvcC for Main 10 with 4-byte NAL lengths.
fn hvcc() -> Vec<u8> {
    let mut c = vec![0u8; 23];
    c[0] = 1;
    c[17] = 0xF8 | 2;
    c[21] = 0xFC | 3;
    c
}

// One length-prefixed HEVC access unit: an optional HDR10+ SEI, then a slice.
pub(crate) fn hevc_frame(hdr10plus: bool) -> Vec<u8> {
    let nal = |n: &[u8]| {
        let mut v = (n.len() as u32).to_be_bytes().to_vec();
        v.extend_from_slice(n);
        v
    };
    let mut out = Vec::new();
    // A mastering-display SEI first, carrying an emulation-prevention byte.
    out.extend(nal(&[
        0x4E, 0x01, 137, 4, 0x00, 0x00, 0x03, 0x01, 0x02, 0x80,
    ]));
    if hdr10plus {
        let mut sei = vec![
            0x4E, 0x01, 4, 8, 0xB5, 0x00, 0x3C, 0x00, 0x01, 0x04, 0x01, 0x40,
        ];
        sei.push(0x80);
        out.extend(nal(&sei));
    }
    out.extend(nal(&[0x26, 0x01, 0xAF, 0x00, 0x11]));
    out
}

fn uhd(colour: &[Vec<u8>], more: &[Vec<u8>]) -> Vec<u8> {
    let mut m = vec![el(0x63A2, &hvcc()), video(3840, 2160, colour)];
    m.extend_from_slice(more);
    entry(1, 1, "V_MPEGH/ISO/HEVC", &m)
}

fn pq() -> Vec<Vec<u8>> {
    vec![
        uint_el(0x55B1, 9),
        uint_el(0x55BA, 16),
        uint_el(0x55BB, 9),
        uint_el(0x55BC, 1000),
        uint_el(0x55BD, 400),
    ]
}

fn truehd(atmos: bool) -> Vec<u8> {
    let mut au = vec![0x00, 0x00, 0x00, 0x00, 0xF8, 0x72, 0x6F, 0xBA];
    au.extend([0u8; 28]);
    au[4 + 16] = if atmos { 0x40 } else { 0x30 };
    au
}

fn dts(exss: bool, xll: bool) -> Vec<u8> {
    let mut f = vec![0x7F, 0xFE, 0x80, 0x01];
    f.extend([0u8; 60]);
    if exss {
        f.extend([0x64, 0x58, 0x20, 0x25]);
        f.extend([0u8; 20]);
        f.extend(if xll {
            [0x41, 0xA2, 0x95, 0x47]
        } else {
            [0x65, 0x5E, 0x31, 0x5E]
        });
        f.extend([0u8; 20]);
    }
    f
}

// Bits, most significant first, for hand-built E-AC-3 headers.
struct W(Vec<u8>, usize);
impl W {
    fn put(&mut self, n: usize, v: u32) -> &mut Self {
        for i in (0..n).rev() {
            if self.1.is_multiple_of(8) {
                self.0.push(0);
            }
            let bit = ((v >> i) & 1) as u8;
            let last = self.0.len() - 1;
            self.0[last] |= bit << (7 - self.1 % 8);
            self.1 += 1;
        }
        self
    }
}

// A 5.1 independent E-AC-3 frame; `mix` exercises the mixing-metadata branch.
pub(crate) fn eac3(joc: bool, mix: bool) -> Vec<u8> {
    let mut w = W(vec![0x0B, 0x77], 16);
    w.put(2, 0).put(3, 0).put(11, 0x2FF).put(2, 0).put(2, 3);
    w.put(3, 7).put(1, 1).put(5, 16);
    w.put(5, 27).put(1, 1).put(8, 0x55);
    if mix {
        w.put(1, 1);
        w.put(2, 1).put(6, 0x2A).put(6, 0x15);
        w.put(1, 1).put(5, 3);
        w.put(1, 1).put(6, 9);
        w.put(1, 0);
        w.put(2, 3).put(5, 1).put(24, 0xABCDEF);
        w.put(1, 1)
            .put(1, 1)
            .put(5, 1)
            .put(1, 0)
            .put(1, 1)
            .put(5, 2);
        w.put(1, 0).put(1, 0).put(1, 0).put(1, 0);
        w.put(1, 0);
    } else {
        w.put(1, 0);
    }
    w.put(1, 1)
        .put(3, 0)
        .put(2, 3)
        .put(2, 1)
        .put(1, 0)
        .put(1, 1);
    w.put(1, 1)
        .put(6, 1)
        .put(7, 0)
        .put(1, u32::from(joc))
        .put(8, 16);
    w.put(32, 0);
    w.0
}

#[test]
fn hdr10_and_dolby_vision_come_from_the_headers() {
    let d = detail(mkv(&[uhd(&pq(), &[dv(8, 1)])], &[], 7_200_000.0));
    assert_eq!(d.tier.as_deref(), Some("uhd"));
    assert_eq!(d.hdr.format, "hdr10");
    let rec = d.hdr.dv.unwrap();
    assert_eq!((rec.profile, rec.level, rec.compat_id), (8, 6, 1));
    assert!(rec.rpu && rec.bl && !rec.el);
    let v = &d.video[0];
    assert_eq!(
        (v.width, v.height, v.bit_depth),
        (Some(3840), Some(2160), Some(10))
    );
    assert_eq!(
        (v.max_cll, v.max_fall, v.transfer),
        (Some(1000), Some(400), Some(16))
    );
    assert_eq!(d.hdr.hdr10plus, None, "no frame was read");

    let p5 = detail(mkv(&[uhd(&pq(), &[dv(5, 0)])], &[], 1000.0));
    assert_eq!(p5.hdr.format, "dv", "profile 5 has no HDR10 fallback");
    let hlg = detail(mkv(&[uhd(&[uint_el(0x55BA, 18)], &[])], &[], 1000.0));
    assert_eq!((hlg.hdr.format.as_str(), hlg.hdr.dv), ("hlg", None));
    let sdr = detail(mkv(&[uhd(&[uint_el(0x55BA, 1)], &[])], &[], 1000.0));
    assert_eq!(sdr.hdr.format, "sdr");
    let bare = detail(mkv(&[uhd(&[], &[])], &[], 1000.0));
    assert_eq!(
        bare.hdr.format, "unknown",
        "no Colour element: never guessed"
    );
    let unspecified = detail(mkv(&[uhd(&[uint_el(0x55BA, 2)], &[])], &[], 1000.0));
    assert_eq!(unspecified.hdr.format, "unknown");
}

#[test]
fn hdr10_plus_is_read_from_the_first_frame() {
    let with = detail(mkv(&[uhd(&pq(), &[])], &[(1, 0, hevc_frame(true))], 1000.0));
    assert_eq!(with.hdr.hdr10plus, Some(true));
    let without = detail(mkv(
        &[uhd(&pq(), &[])],
        &[(1, 0, hevc_frame(false))],
        1000.0,
    ));
    assert_eq!(without.hdr.hdr10plus, Some(false));
    // A frame cut before its first slice says nothing.
    let cut = hevc_frame(false);
    assert_eq!(hevc_hdr10plus(&cut[..14], 4), None);
    assert_eq!(unescape(&[0, 0, 3, 1, 0, 0, 3]), [0, 0, 1, 0, 0]);
}

#[test]
fn the_tier_follows_the_frame_size() {
    let t = |w, h| {
        tier(&VideoTrack {
            width: Some(w),
            height: Some(h),
            ..VideoTrack::default()
        })
    };
    assert_eq!(t(3840, 1600).as_deref(), Some("uhd"), "a cropped scope UHD");
    assert_eq!(t(1920, 800).as_deref(), Some("bluray"));
    assert_eq!(t(1920, 1080).as_deref(), Some("bluray"));
    assert_eq!(t(720, 480).as_deref(), Some("sd"));
    assert_eq!(t(0, 0), None);
}

#[test]
fn audio_is_classified_from_codec_and_bitstream() {
    let named = |n: &str| el(0x536E, n.as_bytes());
    let d = detail(mkv(
        &[
            uhd(&pq(), &[]),
            entry(2, 2, "A_TRUEHD", &[audio(8)]),
            entry(3, 2, "A_DTS", &[audio(6), named("DTS-HD MA 5.1")]),
            entry(4, 2, "A_DTS", &[audio(6)]),
            entry(5, 2, "A_DTS", &[audio(6), named("DTS-HD Master Audio")]),
            entry(6, 2, "A_EAC3", &[audio(6)]),
            entry(7, 2, "A_AC3", &[audio(6)]),
            entry(8, 2, "A_AAC", &[audio(2)]),
            entry(9, 2, "A_PCM/INT/LIT", &[audio(2)]),
            entry(10, 2, "A_EAC3", &[audio(6)]),
            entry(11, 2, "A_TRUEHD", &[audio(6)]),
        ],
        &[
            (2, 0, truehd(true)),
            (3, 0, dts(true, true)),
            (4, 0, dts(true, false)),
            (5, 0, dts(false, false)),
            (6, 0, eac3(true, false)),
            (10, 0, eac3(false, true)),
            (11, 0, truehd(false)),
        ],
        1000.0,
    ));
    let f = |i: usize| {
        let a = &d.audio[i];
        (a.format.as_str(), a.lossless, a.atmos, a.channels)
    };
    assert_eq!(f(0), ("TrueHD", Some(true), Some(true), Some(8)));
    assert_eq!(f(1), ("DTS-HD MA", Some(true), Some(false), Some(6)));
    assert_eq!(f(2), ("DTS-HD HRA", Some(false), Some(false), Some(6)));
    assert_eq!(f(3), ("DTS", Some(false), Some(false), Some(6)));
    assert_eq!(f(4), ("E-AC-3", Some(false), Some(true), Some(6)));
    assert_eq!(f(5), ("AC-3", Some(false), Some(false), Some(6)));
    assert_eq!(f(6), ("AAC", Some(false), Some(false), Some(2)));
    assert_eq!(f(7), ("PCM", Some(true), Some(false), Some(2)));
    assert_eq!(
        f(8),
        ("E-AC-3", Some(false), Some(false), Some(6)),
        "mixing metadata walked"
    );
    assert_eq!(f(9), ("TrueHD", Some(true), Some(false), Some(6)));
    assert!(d.audio[3].lossy_claim, "a DTS core titled as Master Audio");
    assert!(!d.audio[1].lossy_claim && !d.audio[0].lossy_claim);
    assert_eq!(d.best_audio, Some(0));
    assert!(!d.default_not_best);
}

#[test]
fn an_unread_bitstream_leaves_atmos_and_dts_unknown() {
    let d = detail(mkv(
        &[
            uhd(&pq(), &[]),
            entry(2, 2, "A_TRUEHD", &[audio(8)]),
            entry(3, 2, "A_DTS", &[audio(6)]),
        ],
        &[],
        1000.0,
    ));
    assert_eq!((d.audio[0].lossless, d.audio[0].atmos), (Some(true), None));
    assert_eq!(
        (d.audio[1].format.as_str(), d.audio[1].lossless),
        ("DTS", None)
    );
    assert_eq!(d.radar, ["DV"]);
    assert_eq!(d.radar_unknown, ["Atmos"]);
}

#[test]
fn a_default_track_worse_than_the_best_is_flagged() {
    let not_default = uint_el(0x88, 0);
    let d = detail(mkv(
        &[
            entry(
                1,
                1,
                "V_MPEG4/ISO/AVC",
                &[video(1920, 1080, &[uint_el(0x55BA, 1)])],
            ),
            entry(2, 2, "A_AC3", &[audio(6)]),
            entry(3, 2, "A_TRUEHD", &[audio(8), not_default.clone()]),
            entry(
                4,
                17,
                "S_HDMV/PGS",
                &[uint_el(0x55AA, 1), el(0x22_B59C, b"fra")],
            ),
        ],
        &[(3, 0, truehd(false))],
        1000.0,
    ));
    assert_eq!((d.default_audio, d.best_audio), (Some(0), Some(1)));
    assert!(d.default_not_best);
    assert_eq!(d.radar, ["UHD"]);
    let s = &d.subtitles[0];
    assert_eq!(
        (s.format.as_str(), s.language.as_str(), s.forced, s.default),
        ("PGS", "fra", true, true)
    );
    // Two equal tracks: the default is as good as the best, not worse.
    let tie = detail(mkv(
        &[
            entry(1, 1, "V_MPEG4/ISO/AVC", &[video(1920, 1080, &[])]),
            entry(2, 2, "A_AC3", &[audio(6), not_default]),
            entry(3, 2, "A_AC3", &[audio(6)]),
        ],
        &[],
        1000.0,
    ));
    assert_eq!((tie.default_audio, tie.best_audio), (Some(1), Some(0)));
    assert!(!tie.default_not_best);
}

#[test]
fn the_radar_is_per_tier() {
    let audio_of = |format: &str, lossless: bool, atmos: bool, ch: u8| AudioTrack {
        format: format.into(),
        lossless: Some(lossless),
        atmos: Some(atmos),
        channels: Some(ch),
        ..AudioTrack::default()
    };
    let hdr10 = Hdr {
        format: "hdr10".into(),
        ..Hdr::default()
    };
    let dv = Hdr {
        dv: Some(DolbyVision::default()),
        ..hdr10.clone()
    };
    let r = |tier, hdr: &Hdr, a: &[AudioTrack]| radar(Some(tier), hdr, a).0;
    assert_eq!(
        r("uhd", &hdr10, &[audio_of("DTS-HD MA", true, false, 6)]),
        ["DV", "Atmos", "7.1"]
    );
    assert!(r("uhd", &dv, &[audio_of("TrueHD", true, true, 8)]).is_empty());
    assert_eq!(
        r(
            "uhd",
            &dv,
            &[
                audio_of("TrueHD", true, false, 8),
                audio_of("E-AC-3", false, true, 6)
            ]
        ),
        Vec::<String>::new(),
        "Atmos on any track counts"
    );
    assert_eq!(
        r("bluray", &hdr10, &[audio_of("AC-3", false, false, 6)]),
        ["lossless", "UHD"]
    );
    assert_eq!(r("sd", &hdr10, &[audio_of("PCM", true, false, 2)]), ["UHD"]);
    assert_eq!(radar(None, &hdr10, &[]), (vec![], vec![]));
}

#[test]
fn the_timeline_compares_the_last_frame_with_the_declared_length() {
    let v = entry(1, 1, "V_MPEG4/ISO/AVC", &[video(1920, 1080, &[])]);
    let blocks = [(1, 0, vec![0]), (1, 9_960, vec![0]), (1, 5_000, vec![0])];
    let ok = detail(mkv(std::slice::from_ref(&v), &blocks, 10_000.0))
        .timeline
        .unwrap();
    assert_eq!(ok.state, "ok");
    assert_eq!(ok.last_frame_secs, Some(9.96));
    assert_eq!(ok.last_cue_secs, Some(0.0));
    let short = detail(mkv(std::slice::from_ref(&v), &blocks, 30_000.0))
        .timeline
        .unwrap();
    assert_eq!(short.state, "short");
    assert!((short.delta_secs.unwrap() + 20.04).abs() < 1e-9);
    let over = detail(mkv(&[v], &blocks, 5_000.0)).timeline.unwrap();
    assert_eq!(over.state, "overrun");
}

#[test]
fn the_raw_report_lists_every_field() {
    let d = detail(mkv(&[uhd(&pq(), &[dv(8, 1)])], &[], 1000.0));
    assert_eq!(d.raw[0].title, "Segment");
    assert!(
        d.raw[0]
            .fields
            .contains(&("WritingApp".into(), "freemkv 1.7.7".into()))
    );
    assert_eq!(d.raw[1].title, "Track 1 (video)");
    let f = &d.raw[1].fields;
    assert!(f.contains(&(
        "Video › Colour › TransferCharacteristics".into(),
        "16".into()
    )));
    assert!(f.contains(&(
        "BlockAdditionMapping › BlockAddIDType".into(),
        "dvcC".into()
    )));
}

#[test]
fn header_stripped_frames_are_put_back_together() {
    let mut strip = uint_el(0x4254, 3);
    strip.extend(el(0x4255, &[0x0B, 0x77]));
    let enc = el(0x6D80, &el(0x6240, &el(0x5034, &strip)));
    let frame = eac3(true, false)[2..].to_vec();
    let d = detail(mkv(
        &[
            entry(1, 1, "V_MPEG4/ISO/AVC", &[video(720, 480, &[])]),
            entry(2, 2, "A_EAC3", &[audio(6), enc]),
        ],
        &[(2, 0, frame)],
        1000.0,
    ));
    assert_eq!(d.audio[0].atmos, Some(true));
    assert_eq!(d.tier.as_deref(), Some("sd"));
}

#[test]
fn a_file_that_is_not_matroska_has_an_empty_detail_and_storage_errors_none() {
    let t = tempfile::tempdir().unwrap();
    let junk = t.path().join("junk.mkv");
    std::fs::write(&junk, [0x47u8; 64]).unwrap();
    assert_eq!(read(&junk), Some(empty()));
    assert_eq!(read(&t.path().join("gone.mkv")), None);
    let dir = t.path().join("dir.mkv");
    std::fs::create_dir(&dir).unwrap();
    assert_eq!(read(&dir), None);
}
