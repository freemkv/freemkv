//! Synthetic scanned titles for frontend selection-policy tests, not producer validation.

use libfreemkv::disc::ContentSegment;
use libfreemkv::*;

/// Two cuts of one programme, title 0 English and title 1 German: shared clips "a" and
/// "c" around a per-language "b-<lang>", as the scanner describes them.
pub(crate) fn variant_titles() -> Vec<DiscTitle> {
    titles(|language| vec!["a".into(), format!("b-{language}"), "c".into()])
}

/// The same two titles with no content in common: two separate programmes.
pub(crate) fn separate_titles() -> Vec<DiscTitle> {
    titles(|language| vec![format!("whole-{language}")])
}

fn titles(clips: fn(&str) -> Vec<String>) -> Vec<DiscTitle> {
    ["eng", "deu"]
        .into_iter()
        .enumerate()
        .map(|(index, language)| {
            let id = (2 - index) as u16;
            let mut title = DiscTitle::empty();
            title.playlist_id = id;
            title.playlist = format!("Title {id}");
            title.duration_secs = 1000.0 + id as f64;
            title.clips = clips(language)
                .into_iter()
                .map(|clip_id| Clip {
                    marks_on_pes_clock: true,
                    clip_id,
                    in_time: 0,
                    out_time: 45_000_000,
                    duration_secs: 1000.0,
                    source_packets: 0,
                    feed_span: None,
                })
                .collect();
            title.selection_evidence.segments = ContentSegment::from_clips(&title.clips);
            title.streams.push(Stream::Audio(AudioStream {
                pid: 0xbd80,
                codec: Codec::Ac3,
                channels: AudioChannels::Stereo,
                language: language.into(),
                sample_rate: SampleRate::S48,
                secondary: false,
                purpose: LabelPurpose::Normal,
                label: String::new(),
            }));
            title
        })
        .collect()
}
