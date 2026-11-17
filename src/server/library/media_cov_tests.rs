use super::tests::{el, uint_el};
use super::*;

fn float_el(id: u32, v: f64) -> Vec<u8> {
    el(id, &v.to_be_bytes())
}

fn has(t: &Track, name: &str, value: &str) -> bool {
    t.raw.iter().any(|(n, v)| n == name && v == value)
}

#[test]
fn every_track_field_is_read_and_listed() {
    let mut b = Vec::new();
    for (id, v) in [
        (0xD7u32, 3u64),
        (0x73C5, 5),
        (0x83, 1),
        (0xB9, 1),
        (0x88, 1),
        (0x55AA, 1),
        (0x55AB, 1),
        (0x55AC, 0),
        (0x55AD, 0),
        (0x55AE, 1),
        (0x55AF, 0),
        (0x9C, 0),
        (0x23_E383, 41_708_333),
        (0x56AA, 7),
        (0x56BB, 8),
        (0x55EE, 1),
        (0xB0, 1920),
        (0xBA, 1080),
        (0x54B0, 1920),
        (0x54BA, 1080),
        (0x54B2, 0),
        (0x9A, 1),
        (0x9D, 9),
        (0x53B8, 0),
        (0x9F, 6),
        (0x6264, 24),
        (0x55B1, 9),
        (0x55B2, 10),
        (0x55B9, 1),
        (0x55BA, 16),
        (0x55BB, 9),
        (0x55BC, 1000),
        (0x55BD, 400),
        (0x41E7, 1),
        (0x41F0, 4),
    ] {
        b.extend(uint_el(id, v));
    }
    for (id, v) in [
        (0x2383E3u32, 23.976),
        (0xB5, 48000.0),
        (0x78B5, 96000.0),
        (0x55D9, 1000.0),
        (0x55DA, 0.005),
        (0x55D1, 0.68),
    ] {
        b.extend(float_el(id, v));
    }
    b.extend(el(0x536E, b"Main"));
    b.extend(el(0x22_B59C, b"ger"));
    b.extend(el(0x22_B59D, b"de-AT"));
    b.extend(el(0x86, b"V_MPEG4/ISO/AVC"));
    b.extend(el(0x63A2, &[1, 100, 0, 0]));
    b.extend(el(0x25_8688, b"AVC"));
    b.extend(el(0x41ED, &[0xAB, 0xCD]));
    b.extend(el(0x41A4, b"extra"));
    b.extend(el(0xEA, &[7]));
    let t = Track::parse(&b);
    assert_eq!((t.number, t.kind), (3, 1));
    assert!(t.default && t.forced);
    assert_eq!(t.default_duration, Some(41_708_333));
    assert_eq!(
        (t.width, t.height, t.interlaced),
        (Some(1920), Some(1080), Some(1))
    );
    assert_eq!((t.channels, t.bit_depth), (Some(6), Some(24)));
    assert_eq!(t.sample_rate, Some(48000.0));
    assert_eq!(t.name, "Main");
    assert_eq!(t.language(), "de-AT");
    assert_eq!(t.codec_private, vec![1, 100, 0, 0]);
    let c = t.colour.unwrap();
    assert_eq!(
        (c.matrix, c.bits, c.transfer, c.primaries),
        (Some(9), Some(10), Some(16), Some(9))
    );
    assert_eq!((c.max_cll, c.max_fall), (Some(1000), Some(400)));
    assert_eq!(
        (c.mastering_max, c.mastering_min),
        (Some(1000.0), Some(0.005))
    );
    assert!(has(&t, "TrackUID", "5"));
    assert!(has(&t, "DefaultDuration", "41708333 ns"));
    assert!(has(&t, "CodecPrivate", "4 bytes"));
    assert!(has(&t, "CodecName", "AVC"));
    assert!(has(&t, "CodecDelay", "7 ns"));
    assert!(has(&t, "BlockAddIDType", "1"));
    assert!(has(&t, "BlockAddIDExtraData", "abcd"));
    assert!(has(&t, "BlockAddIDName", "extra"));
    assert!(has(&t, "Chromaticity", "0.6800"));
    assert!(has(&t, "0xEA", "1 bytes"));
    let v = video_track(&t, None);
    assert_eq!(v.fps, Some(23.976));
    assert_eq!(v.interlaced, Some(true));
    assert_eq!(
        v.bit_depth,
        Some(10),
        "BitsPerChannel wins over the profile"
    );
    assert_eq!(v.language, "de-AT");
}

#[test]
fn a_progressive_track_without_colour_takes_its_depth_from_the_profile() {
    let mut b = uint_el(0x9A, 2);
    b.extend(el(0x86, b"V_MPEG4/ISO/AVC"));
    b.extend(el(0x63A2, &[1, 100]));
    let v = video_track(&Track::parse(&b), None);
    assert_eq!(v.interlaced, Some(false));
    assert_eq!(v.bit_depth, Some(8));
    let mut b = uint_el(0x9A, 0);
    b.extend(el(0x86, b"V_MPEG4/ISO/AVC"));
    b.extend(el(0x63A2, &[1, 244]));
    let v = video_track(&Track::parse(&b), None);
    assert_eq!(v.interlaced, None);
    assert_eq!(v.bit_depth, None, "a high profile may be 10-bit");
    assert_eq!(v.language, "eng");
}

#[test]
fn encrypted_and_compressed_tracks_are_opaque() {
    let enc = el(0x6D80, &el(0x6240, &el(0x5035, &uint_el(0x47E1, 5))));
    assert!(Track::parse(&enc).opaque);
    let mut zlib = uint_el(0x4254, 0);
    zlib.extend(uint_el(0x4277, 1));
    let comp = el(0x6D80, &el(0x6240, &el(0x5034, &zlib)));
    let t = Track::parse(&comp);
    assert!(t.opaque && t.strip.is_none());
    assert!(has(
        &t,
        "ContentEncodings › ContentEncoding › ContentCompression › ContentCompAlgo",
        "0"
    ));
}

#[test]
fn segment_info_lists_title_date_and_uuid() {
    let mut b = uint_el(0x2A_D7B1, 0);
    b.extend(el(0x7BA9, b"Heat"));
    b.extend(el(0x4461, &0i64.to_be_bytes()));
    b.extend(el(0x73A4, &[0x01, 0xFE]));
    b.extend(el(0xEC, &[0]));
    let i = Info::parse(&b);
    assert_eq!(i.scale, 1, "a zero scale is taken as one");
    assert!(i.raw.contains(&("Title".into(), "Heat".into())));
    assert!(i.raw.contains(&("SegmentUUID".into(), "01fe".into())));
    let date = &i.raw.iter().find(|(n, _)| n == "DateUTC").unwrap().1;
    assert!(date.starts_with("2001-01-01"), "{date}");
    assert_eq!(i.raw.len(), 4, "an unknown field is not listed");
}

#[test]
fn small_readers_handle_their_edge_cases() {
    assert_eq!(kind_name(99), "other");
    assert_eq!(kind_name(17), "subtitles");
    assert_eq!(DtsKind::Express.name(), "DTS Express");
    assert_eq!(DtsKind::HdHra.name(), "DTS-HD HRA");
    assert_eq!(hevc_nal_length(&[]), 4);
    assert_eq!(float(&[]), Some(0.0));
    assert_eq!(float(&1.5f32.to_be_bytes()), Some(1.5));
    assert_eq!(float(&[1, 2, 3]), None);
    assert_eq!(sei_number(&[0xFF, 3]), Some((258, 2)));
    assert_eq!(sei_number(&[0xFF, 0xFF]), None);
    assert_eq!(vint_id(&[0x00, 1, 2, 3, 4]), None);
    assert_eq!(vint_size(&[0x00]), None);
    assert_eq!(bit_depth("V_MPEG4/ISO/AVC", &[1, 77]), Some(8));
    assert_eq!(bit_depth("V_MS/VFW", &[1, 77]), None);
    let soft_err: io::Result<u32> = soft(Err(io::Error::other("storage")));
    assert!(soft_err.is_err());
    let verdict: io::Result<u32> = soft(Err(io::Error::from(io::ErrorKind::InvalidData)));
    assert_eq!(verdict.unwrap(), 0);
}

#[test]
fn a_parameter_set_before_the_first_slice_is_skipped() {
    // A VPS (type 32), then a slice (type 1): no HDR10+ SEI came first.
    let frame = [0, 0, 0, 2, 0x40, 0x01, 0, 0, 0, 2, 0x02, 0x01];
    assert_eq!(hevc_hdr10plus(&frame, 4), Some(false));
    assert_eq!(hevc_hdr10plus(&frame[..6], 4), None);
}
