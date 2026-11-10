use super::*;
use crate::server::planner::{MediaMetadata, TitleRequest};
use libfreemkv::disc::ContentSegment;
use libfreemkv::{
    AudioChannels, AudioStream, Clip, Codec, ColorSpace, FrameRate, HdrFormat, LabelPurpose,
    Resolution, SampleRate, Stream, VideoStream,
};

// 45 kHz ticks per minute.
const TICKS: u32 = 45_000 * 60;

// A BD playlist `id` playing `(clip, in, out)` items (minutes on the clip's clock).
fn bd(id: u16, minutes: f64, items: &[(&str, u32, u32)]) -> DiscTitle {
    let mut t = DiscTitle::empty();
    t.playlist_id = id;
    t.playlist = format!("{id:05}.mpls");
    t.duration_secs = minutes * 60.0;
    t.size_bytes = (minutes * 1e8) as u64;
    t.clips = (items.iter())
        .map(|&(clip, start, end)| Clip {
            marks_on_pes_clock: true,
            clip_id: clip.into(),
            in_time: start * TICKS,
            out_time: end * TICKS,
            duration_secs: f64::from(end - start) * 60.0,
            source_packets: 0,
            feed_span: None,
        })
        .collect();
    t.selection_evidence.segments = ContentSegment::from_clips(&t.clips);
    t.streams = vec![
        Stream::Video(VideoStream {
            pid: 0x1011,
            codec: Codec::H264,
            resolution: Resolution::R1080p,
            frame_rate: FrameRate::F23_976,
            hdr: HdrFormat::Sdr,
            color_space: ColorSpace::Bt709,
            display_aspect: None,
            secondary: false,
            label: String::new(),
            measured_cicp: None,
        }),
        Stream::Audio(AudioStream {
            pid: 0x1100,
            codec: Codec::Ac3,
            channels: AudioChannels::Stereo,
            language: "eng".into(),
            sample_rate: SampleRate::S48,
            secondary: false,
            purpose: LabelPurpose::Normal,
            label: String::new(),
        }),
    ];
    t
}

// Four episodes cut from one m2ts, its play-all, and an extra; no roster proves them.
pub(crate) fn play_all_disc() -> Vec<DiscTitle> {
    vec![
        bd(800, 176.0, &[("00010", 0, 176)]),
        bd(803, 44.0, &[("00010", 0, 44)]),
        bd(801, 44.0, &[("00010", 88, 132)]),
        bd(804, 44.0, &[("00010", 44, 88)]),
        bd(802, 44.0, &[("00010", 132, 176)]),
        bd(805, 12.0, &[("00020", 0, 12)]),
    ]
}

fn tv() -> TitleRequest {
    TitleRequest {
        media: MediaMetadata {
            kind: Some(MediaKind::Tv),
            ..MediaMetadata::default()
        },
        ..TitleRequest::default()
    }
}

#[test]
fn an_unproven_tv_disc_lists_its_suggestion_and_play_all() {
    let titles = play_all_disc();
    let list = list(&Config::default(), &titles, &tv(), ScopeMode::Mux);
    assert!(
        list.automatic.is_empty(),
        "an unproven set runs nothing on its own"
    );
    let confirm = list.confirm.expect("a suggestion to confirm");
    assert_eq!(confirm.suggested, vec![1, 3, 2, 4]);
    assert_eq!(list.titles.len(), 6);
    assert!(list.titles[0].play_all);
    assert!(list.titles.iter().skip(1).all(|t| !t.play_all));
    assert!(
        list.titles
            .iter()
            .all(|t| !t.in_roster && t.swept.is_none())
    );
    // Each row carries the identity a pick sends back, and what the operator compares.
    assert_eq!(list.titles[2].id, TitleIdentity::of(&titles[2]));
    assert_eq!(list.titles[2].playlist, "00801.mpls");
    assert_eq!(list.titles[2].duration, "0h 44m");
    assert!(
        list.titles[2].tracks.contains("1 audio"),
        "{}",
        list.titles[2].tracks
    );
    // The same disc as a movie runs its main title and suggests nothing.
    let movie = TitleRequest {
        media: MediaMetadata {
            kind: Some(MediaKind::Movie),
            ..MediaMetadata::default()
        },
        ..TitleRequest::default()
    };
    let list = super::list(&Config::default(), &titles, &movie, ScopeMode::Mux);
    assert_eq!(list.automatic.len(), 1);
    assert!(list.confirm.is_none());
}

#[test]
fn a_proven_roster_is_flagged_and_runs_on_its_own() {
    let mut titles = vec![bd(1, 44.0, &[("a", 0, 44)]), bd(2, 44.0, &[("b", 0, 44)])];
    for (index, t) in titles.iter_mut().enumerate() {
        t.selection_evidence.episodes = libfreemkv::disc::EpisodeEvidence::Authored {
            roster: "test-menu".into(),
            title_count: 2,
            ordinal: Some(index),
        };
    }
    let list = list(&Config::default(), &titles, &tv(), ScopeMode::Mux);
    assert_eq!(list.automatic, vec![0, 1]);
    assert!(list.confirm.is_none());
    assert!(list.titles.iter().all(|t| t.in_roster));
}

#[test]
fn a_movie_is_one_title_and_one_episode_needs_its_first_number() {
    assert!(check_pick(MediaKind::Movie, 1, None).is_ok());
    for count in [0, 2] {
        let error = check_pick(MediaKind::Movie, count, None).unwrap_err();
        assert!(error.contains("exactly one"), "{error}");
    }
    assert!(check_pick(MediaKind::Tv, 0, Some(1)).is_err());
    let error = check_pick(MediaKind::Tv, 1, None).unwrap_err();
    assert!(error.contains("First episode"), "{error}");
    assert!(check_pick(MediaKind::Tv, 1, Some(5)).is_ok());
    assert!(check_pick(MediaKind::Tv, 3, None).is_ok());
}

#[test]
fn picks_round_trip_by_identity_and_refuse_anything_else() {
    let titles = play_all_disc();
    let ids: Vec<_> = [3, 1]
        .iter()
        .map(|&i| TitleIdentity::of(&titles[i]))
        .collect();
    let body = serde_json::to_value(&ids).unwrap();
    let pick = parse_pick(&body).unwrap().unwrap();
    assert_eq!(remap(&titles, &pick), Ok(vec![3, 1]), "order is kept");
    assert_eq!(parse_pick(&serde_json::json!([])), Ok(None), "empty clears");
    for bad in [
        serde_json::json!(null),
        serde_json::json!("00801.mpls"),
        serde_json::json!([3]),
        serde_json::json!([{"playlist": "x"}]),
    ] {
        assert!(parse_pick(&bad).is_err(), "{bad}");
    }
    // A title this scan does not have, or one named twice, is a mismatch.
    let mut foreign = titles[2].clone();
    foreign.playlist_id = 999;
    assert!(remap(&titles, &[TitleIdentity::of(&foreign)]).is_err());
    assert!(remap(&titles, &[ids[0].clone(), ids[0].clone()]).is_err());
}
