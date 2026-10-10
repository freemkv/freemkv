use super::{Row, scanned_from_disc, stream_rows};
use crate::strings::is_unsafe_display_char;
use libfreemkv::disc::{BdRegion, DiscRegion};
use libfreemkv::{
    AudioChannels, AudioStream, Codec, ColorSpace, ContentFormat, Disc, DiscFormat, DiscTitle,
    FrameRate, HdrFormat, LabelPurpose, LabelQualifier, Resolution, SampleRate, Stream,
    SubtitleStream, VideoStream,
};

/// A payload that carries one member of every class `is_unsafe_display_char`
/// rejects: a C0 control, an ESC-introduced OSC, a newline (log forging), a
/// bidi override and a zero-width joiner.
const HOSTILE: &str = "a\u{7}b\u{1b}]0;pwned\u{7}c\nLabel: forged\u{202e}e\u{200b}f";

/// The control payload for the tooltip-shape comparison: same field, no
/// character the display rule objects to.
const BENIGN: &str = "abcdef";

fn hostile_disc() -> Disc {
    let video = Stream::Video(VideoStream {
        pid: 0x1011,
        codec: Codec::Hevc,
        resolution: Resolution::Unknown,
        frame_rate: FrameRate::Unknown,
        hdr: HdrFormat::Sdr,
        color_space: ColorSpace::Bt709,
        display_aspect: None,
        secondary: true,
        label: HOSTILE.to_string(),
        measured_cicp: None,
    });
    let audio = Stream::Audio(AudioStream {
        pid: 0x1100,
        codec: Codec::TrueHd,
        channels: AudioChannels::Unknown,
        language: HOSTILE.to_string(),
        sample_rate: SampleRate::Unknown,
        secondary: true,
        purpose: LabelPurpose::Commentary,
        label: HOSTILE.to_string(),
    });
    let subtitle = Stream::Subtitle(SubtitleStream {
        pid: 0x1200,
        codec: Codec::Pgs,
        language: HOSTILE.to_string(),
        forced: true,
        qualifier: LabelQualifier::None,
        codec_data: None,
    });

    Disc {
        volume_id: HOSTILE.to_string(),
        meta_title: None,
        format: DiscFormat::Uhd,
        capacity_sectors: 0,
        capacity_bytes: 0,
        layers: 1,
        titles: vec![DiscTitle {
            selection_evidence: Default::default(),
            playlist: HOSTILE.to_string(),
            playlist_id: 800,
            duration_secs: 60.0,
            size_bytes: 1 << 30,
            clips: Vec::new(),
            streams: vec![video, audio, subtitle],
            chapters: [0.0, 30.0]
                .map(|time_secs| libfreemkv::disc::Chapter {
                    time_secs,
                    name: HOSTILE.to_string(),
                })
                .to_vec(),
            extents: Vec::new(),
            content_format: ContentFormat::BdTs,
            codec_privates: Vec::new(),
        }],
        region: DiscRegion::BluRay(vec![BdRegion::A]),
        aacs: None,
        css: None,
        encrypted: true,
        aacs_error: None,
        css_error: None,
        content_format: ContentFormat::BdTs,
    }
}

// The same disc with a harmless payload, as the control for the
// line-count comparison in tooltips_keep_their_own_shape — newline
// injection into `info` is caught there since its own newlines are legit.
fn benign_disc() -> Disc {
    let mut d = hostile_disc();
    d.volume_id = BENIGN.to_string();
    for t in &mut d.titles {
        t.playlist = BENIGN.to_string();
        for st in &mut t.streams {
            match st {
                Stream::Video(v) => v.label = BENIGN.to_string(),
                Stream::Audio(a) => {
                    a.label = BENIGN.to_string();
                    a.language = BENIGN.to_string();
                }
                Stream::Subtitle(s) => s.language = BENIGN.to_string(),
            }
        }
        for c in &mut t.chapters {
            c.name = BENIGN.to_string();
        }
    }
    d
}

fn offenders(rows: &[Row]) -> Vec<String> {
    let mut bad: Vec<String> = Vec::new();
    for r in rows {
        for s in [&r.desc, &r.type_s, &r.item, &r.format, &r.notes] {
            if s.chars().any(is_unsafe_display_char) {
                bad.push(s.clone());
            }
        }
        if r.info
            .chars()
            .any(|c| c != '\n' && is_unsafe_display_char(c))
        {
            bad.push(r.info.clone());
        }
    }
    bad
}

/// The rows the GUI renders for a whole disc — the title line (playlist)
/// and the disc line (volume id) included.
#[test]
fn no_disc_derived_row_text_carries_an_unsafe_display_char() {
    let disc = hostile_disc();
    let scanned = scanned_from_disc(&disc, "none".into());
    let bad = offenders(&scanned.rows);
    assert!(bad.is_empty(), "unsanitised row text: {bad:?}");
}

/// Every cell the shells paint, chapter rows included, as the core hands them over.
#[test]
fn no_painted_cell_carries_an_unsafe_display_char() {
    let scanned = scanned_from_disc(&hostile_disc(), "none".into());
    assert!(scanned.rows.iter().any(|r| r.type_s == "Chapter"));
    let mut app = crate::ui::App::new();
    app.tree = crate::ui::Tree::from_scan(
        &scanned,
        "All titles",
        0.0,
        &crate::ui::LangPrefs::default(),
    );
    let rows = app.view().title_rows;
    assert!(
        rows.iter().any(|r| r.type_s == "Chapter"),
        "chapters painted"
    );
    assert!(
        rows.iter().any(|r| !r.lang.is_empty()),
        "a Language cell painted"
    );
    let mut bad = Vec::new();
    for r in &rows {
        let cells = [
            &r.type_s, &r.desc, &r.length, &r.size, &r.lang, &r.item, &r.format, &r.notes,
        ];
        bad.extend(
            cells
                .into_iter()
                .filter(|s| s.chars().any(is_unsafe_display_char)),
        );
    }
    assert!(bad.is_empty(), "unsanitised painted cells: {bad:?}");
}

/// A scanned disc's chapters as the shells get them: under each shown title's closed
/// Chapters row, at depth 3, timed, and gone with a title too short to show.
#[test]
fn chapters_hang_under_their_titles_in_the_built_tree() {
    let mut disc = benign_disc();
    let mut short = disc.titles[0].clone();
    short.duration_secs = 10.0;
    let mut long = disc.titles[0].clone();
    long.duration_secs = 120.0;
    long.chapters[1].time_secs = 45.0;
    disc.titles.extend([short, long]);
    let sc = scanned_from_disc(&disc, "none".into());
    let mut app = crate::ui::App::new();
    app.tree =
        crate::ui::Tree::from_scan(&sc, "All titles", 30.0, &crate::ui::LangPrefs::default());

    let arena = &app.tree.arena;
    let parent = |i: usize| arena.iter().position(|n| n.children.contains(&i));
    let chapters: Vec<usize> = (0..arena.len())
        .filter(|&i| arena[i].type_s == "Chapter")
        .collect();
    assert_eq!(chapters.len(), 4, "two shown titles, two chapters each");
    for &c in &chapters {
        let group = parent(c).expect("a chapter is attached");
        assert_eq!(arena[group].type_s, "Chapters");
        let title = parent(group).expect("a chapter list is attached");
        assert_eq!(arena[title].type_s, "Title");
        assert_eq!(arena[c].title_idx, arena[title].title_idx);
    }

    let rows = app.view().title_rows;
    let titles: Vec<&str> = rows
        .iter()
        .filter(|r| r.type_s == "Title")
        .map(|r| r.item.as_str())
        .collect();
    assert_eq!(titles, ["Title 1", "Title 3"], "the short title is hidden");
    let shown: Vec<(u8, &str, bool, bool)> = rows
        .iter()
        .filter(|r| r.type_s.starts_with("Chapter"))
        .map(|r| {
            let closed = crate::ui::starts_collapsed(r);
            (r.depth, r.length.as_str(), closed, r.check.is_none())
        })
        .collect();
    assert_eq!(
        shown,
        [
            (2, "", true, true),
            (3, "0:30", false, true),
            (3, "0:30", false, true),
            (2, "", true, true),
            (3, "0:45", false, true),
            (3, "1:15", false, true),
        ]
    );
    let parents = crate::ui::row_parents(&rows);
    for (i, r) in rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r.type_s == "Chapter")
    {
        let group = parents[i].expect("a chapter row has a parent");
        assert_eq!(rows[group].type_s, "Chapters", "row {i}: {r:?}");
    }
}

/// The stream rows on their own, so a regression in `stream_rows` cannot
/// hide behind a passing disc-level assertion.
#[test]
fn no_stream_row_text_carries_an_unsafe_display_char() {
    let disc = hostile_disc();
    let bad = offenders(&stream_rows(&disc.titles[0], 0));
    assert!(bad.is_empty(), "unsanitised stream row text: {bad:?}");
}

/// M1b: "Its checkbox is **disabled and mirrors the base**" (mpg-output-design v5 §3).
/// The DVD MPEG-2 extension row is no choice of its own and names base PID 0xC0|n; a
/// foreign 0xD3 without the sentinel label stays an ordinary choice.
#[test]
fn an_mp2_extension_stream_row_mirrors_its_base_pid() {
    let mk = |pid: u16, label: &str| {
        Stream::Audio(AudioStream {
            pid,
            codec: Codec::Mp2,
            channels: AudioChannels::Stereo,
            language: "eng".into(),
            sample_rate: SampleRate::S48,
            secondary: false,
            purpose: LabelPurpose::Normal,
            label: label.into(),
        })
    };
    let ext_label = libfreemkv::disc::MP2_EXTENSION_LABEL;
    let mut t = hostile_disc().titles.remove(0);
    t.streams = vec![mk(0xC2, ""), mk(0xD2, ext_label), mk(0xD3, "")];
    let rows = stream_rows(&t, 0);
    assert!(
        rows[0].checkable && rows[0].mirrors.is_none(),
        "{:?}",
        rows[0]
    );
    assert!(!rows[1].checkable, "the extension row is not a choice");
    assert_eq!(rows[1].mirrors, Some(0xC2));
    assert_eq!(rows[1].pid, Some(0xD2));
    assert!(
        rows[1].desc.contains(ext_label),
        "shown with its label: {}",
        rows[1].desc
    );
    assert!(
        rows[2].checkable && rows[2].mirrors.is_none(),
        "{:?}",
        rows[2]
    );
}

/// A crafted field must not be able to add a LINE to a tooltip — the one
/// unsafe character `offenders` cannot judge inside `info`. Same disc,
/// same shape, only the payload differs, so any extra line came from it.
#[test]
fn tooltips_keep_their_own_shape() {
    let hostile = scanned_from_disc(&hostile_disc(), "none".into());
    let benign = scanned_from_disc(&benign_disc(), "none".into());
    assert_eq!(hostile.rows.len(), benign.rows.len(), "row count differs");
    for (h, b) in hostile.rows.iter().zip(&benign.rows) {
        assert_eq!(
            h.info.lines().count(),
            b.info.lines().count(),
            "tooltip gained a line from the payload:\n{}",
            h.info
        );
    }
}

/// The Length and Size columns read typed fields: a title row carries its
/// running time and size as data, and neither as Description text.
#[test]
fn a_title_row_carries_its_size_as_data_not_description_text() {
    let mut disc = benign_disc();
    let t = &mut disc.titles[0];
    t.playlist = "VTS_01_2.VOB".into();
    t.duration_secs = 8600.0;
    t.size_bytes = 6_800_000_000;
    t.chapters = (0..19)
        .map(|i| libfreemkv::disc::Chapter {
            time_secs: f64::from(i) * 400.0,
            name: (i + 1).to_string(),
        })
        .collect();
    let rows = scanned_from_disc(&disc, "none".into()).rows;
    let disc_row = &rows[0];
    assert_eq!(
        disc_row.desc, BENIGN,
        "the disc row shows only the volume name"
    );
    assert_eq!(disc_row.size_bytes, None);
    let title = &rows[1];
    assert_eq!(title.item, "Title 1");
    assert!(title.notes.ends_with("19 chapters"), "{}", title.notes);
    assert_eq!(title.duration_secs, 8600.0);
    assert_eq!(title.size_bytes, Some(6_800_000_000));
    for r in rows.iter().filter(|r| r.depth == 2) {
        assert_eq!(r.size_bytes, None, "{r:?}");
    }
}

#[test]
fn a_title_row_names_its_number_and_counts_chapters_in_the_right_number() {
    assert_eq!(super::title_item(0), "Title 1");
    assert_eq!(super::chapters_note(1), "1 chapter");
    assert_eq!(super::chapters_note(0), "0 chapters");
    let mut t = libfreemkv::DiscTitle::empty();
    t.playlist = "00800.mpls".into();
    t.chapters = vec![
        libfreemkv::disc::Chapter {
            time_secs: 0.0,
            name: "1".into()
        };
        12
    ];
    assert_eq!(
        super::title_notes(&t, true, None),
        "00800.mpls · 12 chapters"
    );
    // A DVD's made-up playlist name is never shown.
    assert_eq!(super::title_notes(&t, false, None), "12 chapters");
}

#[test]
fn chapter_rows_name_only_what_the_disc_names_and_time_each_chapter() {
    let ch = |time_secs: f64, name: &str| libfreemkv::disc::Chapter {
        time_secs,
        name: name.into(),
    };
    let mut t = libfreemkv::DiscTitle::empty();
    t.duration_secs = 300.0;
    t.chapters = vec![ch(0.0, "eps1_1"), ch(174.67, "2"), ch(264.66, "1_show")];
    let rows = super::chapter_rows(&t, 4);
    let shape: Vec<_> = rows
        .iter()
        .map(|r| {
            (
                r.type_s.as_str(),
                r.depth,
                r.item.as_str(),
                r.notes.as_str(),
                r.duration_secs,
            )
        })
        .collect();
    assert_eq!(
        shape,
        [
            ("Chapters", 2, "Chapters", "", 0.0),
            ("Chapter", 3, "Chapter 1", "eps1_1", 175.0),
            // A bare ordinal (a Blu-ray's, or a DVD without text data) is no name.
            ("Chapter", 3, "Chapter 2", "", 90.0),
            ("Chapter", 3, "Chapter 3", "1_show", 35.0),
        ]
    );
    assert!(
        rows.iter()
            .all(|r| !r.checkable && r.pid.is_none() && r.title == 4)
    );
    t.chapters.truncate(1);
    assert!(
        super::chapter_rows(&t, 0).is_empty(),
        "one chapter is no list"
    );
}

/// The payload has to be able to fail the assertion — a filter that
/// silently passed everything would make both tests above vacuous.
#[test]
fn the_hostile_payload_is_actually_unsafe() {
    assert!(HOSTILE.chars().any(is_unsafe_display_char));
}
