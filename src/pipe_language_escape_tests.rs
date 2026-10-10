use super::render_stream_sel_error;

fn title_with_language(lang: &str) -> libfreemkv::DiscTitle {
    libfreemkv::DiscTitle {
        selection_evidence: Default::default(),
        playlist: "00800.mpls".into(),
        playlist_id: 800,
        duration_secs: 60.0,
        size_bytes: 1 << 30,
        clips: Vec::new(),
        streams: vec![libfreemkv::Stream::Audio(libfreemkv::AudioStream {
            pid: 0x1100,
            codec: libfreemkv::Codec::TrueHd,
            channels: libfreemkv::AudioChannels::Unknown,
            language: lang.to_string(),
            sample_rate: libfreemkv::SampleRate::Unknown,
            secondary: false,
            purpose: libfreemkv::LabelPurpose::Normal,
            label: String::new(),
        })],
        chapters: Vec::new(),
        extents: Vec::new(),
        content_format: libfreemkv::ContentFormat::BdTs,
        codec_privates: Vec::new(),
    }
}

/// The "languages available on this title" list is disc bytes.
#[test]
fn a_crafted_language_tag_cannot_reach_the_terminal_raw() {
    // ESC c is RIS: a full terminal reset, and it fits in three bytes.
    let hostile = "\u{1b}c\n";
    let title = title_with_language(hostile);
    let msg = render_stream_sel_error(
        &freemkv_engine::StreamSelError::UnknownLanguage { tag: "zz".into() },
        &title,
    );
    assert!(
        !msg.chars().any(|c| c.is_control() && c != '\n'),
        "an escape sequence survived into terminal output: {msg:?}"
    );
    assert!(
        !msg.contains('\u{1b}'),
        "ESC survived into terminal output: {msg:?}"
    );
}

/// An ordinary tag still names the language it was there to name.
#[test]
fn an_ordinary_language_tag_is_still_shown() {
    let title = title_with_language("eng");
    let msg = render_stream_sel_error(
        &freemkv_engine::StreamSelError::UnknownLanguage { tag: "zz".into() },
        &title,
    );
    assert!(msg.contains("eng"), "got {msg:?}");
}

/// The title loop prefixes the level word when it renders the failure, so the
/// message itself carries none ("Error: Error: ...").
#[test]
fn an_unknown_language_message_carries_no_level_word() {
    let title = title_with_language("eng");
    let msg = render_stream_sel_error(
        &freemkv_engine::StreamSelError::UnknownLanguage { tag: "zz".into() },
        &title,
    );
    let level = crate::strings::get(crate::messaging::Level::Error.locale_key());
    assert_eq!(
        super::render_error(&msg).matches(&level).count(),
        1,
        "{msg}"
    );
}
