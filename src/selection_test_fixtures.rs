//! Synthetic launch evidence for frontend policy tests, not producer validation.

pub(crate) fn launch_titles() -> Vec<libfreemkv::DiscTitle> {
    use libfreemkv::disc::{DvdLaunchEvidence, DvdLaunchRoute, DvdLaunchStep};
    use libfreemkv::*;
    ["eng", "deu"]
        .into_iter()
        .enumerate()
        .map(|(index, language)| {
            let id = (2 - index) as u16;
            let mut title = DiscTitle::empty();
            title.playlist_id = id;
            title.playlist = format!("Title {id}");
            title.duration_secs = 1000.0 + id as f64;
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
            title.selection_evidence.dvd_launch = DvdLaunchEvidence::VerifiedRoot {
                vts: 1,
                pgcn: 1,
                title_count: 2,
                routes: vec![DvdLaunchRoute {
                    button: id as u8,
                    display_masks: vec![1, 4],
                    target_vts: 1,
                    target_title: id as u8,
                    target_part: 1,
                    audio_stream: id as u8 - 1,
                    audio_pid: 0xbd80,
                    audio_language: language.into(),
                    traces: vec![vec![DvdLaunchStep {
                        vts: 1,
                        menu_vob: true,
                        byte_offset: 197,
                        command: [0x30, 5, 0, 1, 0, id as u8, 0, 0],
                    }]],
                }],
            };
            title
        })
        .collect()
}
