fn el(id: &[u8], body: &[u8]) -> Vec<u8> {
    let mut out = id.to_vec();
    out.push(0x01);
    out.extend_from_slice(&(body.len() as u64).to_be_bytes()[1..]);
    out.extend_from_slice(body);
    out
}

pub(crate) fn mkv(
    app: &str,
    duration_secs: Option<f64>,
    cue_secs: Option<u64>,
    video: bool,
) -> Vec<u8> {
    let mut entry = el(&[0xD7], &[1]);
    entry.extend(el(&[0x83], &[if video { 1 } else { 2 }]));
    entry.extend(el(&[0x86], b"V_MPEG4/ISO/AVC"));
    build(app, duration_secs, cue_secs, &[entry])
}

/// Like [`mkv`], with one track per `(type, codec id, language)`: type 1 video, 2 audio,
/// 17 subtitle.
pub(crate) fn mkv_tracks(
    app: &str,
    duration_secs: Option<f64>,
    cue_secs: Option<u64>,
    tracks: &[(u8, &str, &str)],
) -> Vec<u8> {
    let entries: Vec<Vec<u8>> = tracks
        .iter()
        .enumerate()
        .map(|(i, (kind, codec, lang))| {
            let mut entry = el(&[0xD7], &[i as u8 + 1]);
            entry.extend(el(&[0x83], &[*kind]));
            entry.extend(el(&[0x86], codec.as_bytes()));
            entry.extend(el(&[0x22, 0xB5, 0x9C], lang.as_bytes()));
            entry
        })
        .collect();
    build(app, duration_secs, cue_secs, &entries)
}

fn build(
    app: &str,
    duration_secs: Option<f64>,
    cue_secs: Option<u64>,
    entries: &[Vec<u8>],
) -> Vec<u8> {
    let mut info = el(&[0x2A, 0xD7, 0xB1], &1_000_000u64.to_be_bytes());
    if let Some(d) = duration_secs {
        info.extend(el(&[0x44, 0x89], &(d * 1000.0).to_be_bytes()));
    }
    info.extend(el(&[0x4D, 0x80], app.as_bytes()));
    info.extend(el(&[0x57, 0x41], app.as_bytes()));
    let tracks: Vec<u8> = entries.iter().flat_map(|e| el(&[0xAE], e)).collect();
    let mut body = el(&[0x15, 0x49, 0xA9, 0x66], &info);
    body.extend(el(&[0x16, 0x54, 0xAE, 0x6B], &tracks));
    if let Some(t) = cue_secs {
        let point = el(&[0xB3], &(t * 1000).to_be_bytes());
        body.extend(el(&[0x1C, 0x53, 0xBB, 0x6B], &el(&[0xBB], &point)));
    }
    let mut out = el(&[0x1A, 0x45, 0xDF, 0xA3], &[]);
    out.extend(el(&[0x18, 0x53, 0x80, 0x67], &body));
    out
}
