use super::{
    TitleIdentity, disc_title_nums, job_identity, resolve_disc_all_titles, resolve_scanned_title,
    title_changed_message,
};

#[test]
fn a_failed_all_titles_scan_aborts_instead_of_ripping_title_one() {
    let scan = first_scan();
    let ids: Vec<TitleIdentity> = scan.iter().map(TitleIdentity::of).collect();

    // Scan succeeded: `-t all` expands to every scanned title, identities
    // carried through so each job can verify against its own title.
    let (nums, got_ids) =
        resolve_disc_all_titles(&[], Some(ids.clone())).expect("a good scan proceeds");
    assert_eq!(nums, vec![1, 2, 3, 4]);
    assert_eq!(got_ids, ids);

    // Scan FAILED: the resolver returns None so the caller aborts loudly.
    // Anything other than None here is the resurrected rc-0-over-title-1 bug.
    assert!(
        resolve_disc_all_titles(&[], None).is_none(),
        "a failed -t all scan must abort, not silently rip one title"
    );
}

fn title(playlist_id: u16, start_lba: u32) -> libfreemkv::DiscTitle {
    libfreemkv::DiscTitle {
        playlist: format!("{:05}.mpls", playlist_id),
        playlist_id,
        duration_secs: 3600.0,
        size_bytes: 20 << 30,
        clips: Vec::new(),
        streams: Vec::new(),
        chapters: Vec::new(),
        extents: vec![libfreemkv::Extent {
            start_lba,
            sector_count: 1000,
        }],
        content_format: libfreemkv::ContentFormat::BdTs,
        codec_privates: Vec::new(),
    }
}

/// The first scan: what the job list was built against.
fn first_scan() -> Vec<libfreemkv::DiscTitle> {
    vec![
        title(800, 1000),
        title(801, 5000),
        title(802, 9000),
        title(803, 13000),
    ]
}

#[test]
fn a_reordered_rescan_fails_loudly_instead_of_muxing_the_wrong_title() {
    let expected = TitleIdentity::of(&first_scan()[1]);
    let rescan = vec![
        title(800, 1000),
        title(802, 9000), // ← swapped with 801
        title(801, 5000),
        title(803, 13000),
    ];
    let err = resolve_scanned_title(&rescan, 1, Some(&expected))
        .err()
        .unwrap_or_else(|| {
            panic!(
                "a reordered re-scan resolved index 1 to {}, and the rip would have muxed \
                     it as title 2",
                rescan[1].playlist
            )
        });
    assert!(
        err.contains("00801.mpls") && err.contains("00802.mpls"),
        "the error must name the title that was expected and the one found: {err:?}"
    );
}

/// The other half of what `title_in_range` misses: the list SHRANK, but not
/// enough for the index to fall off the end. Dropping 801 slides 803 into
/// index 2, so the old range check passes and the wrong title is muxed.
#[test]
fn a_rescan_that_drops_an_earlier_title_fails_rather_than_shifting() {
    let expected = TitleIdentity::of(&first_scan()[2]);
    let rescan = vec![title(800, 1000), title(802, 9000), title(803, 13000)];
    assert!(
        resolve_scanned_title(&rescan, 2, Some(&expected)).is_err(),
        "index 2 is in range but now names 00803.mpls, not the requested 00802.mpls"
    );
}

/// The identity must not be defined in terms of the OTHER titles, or a disc
/// whose re-scan simply misses one trailing title would fail every job. The
/// requested title is still at its index and still itself: rip it.
#[test]
fn dropping_an_unrelated_later_title_still_rips_the_requested_one() {
    let expected = TitleIdentity::of(&first_scan()[1]);
    let rescan = vec![title(800, 1000), title(801, 5000), title(802, 9000)];
    let got = resolve_scanned_title(&rescan, 1, Some(&expected))
        .expect("the requested title is unchanged; dropping 00803.mpls is irrelevant to it");
    assert_eq!(got.playlist, "00801.mpls");
}

/// THE NORMAL PATH. A stable disc re-scans identically, and every title
/// resolves to exactly the one its job was built for.
#[test]
fn a_stable_disc_resolves_every_title_exactly_as_before() {
    let scan = first_scan();
    for (idx, want) in scan.iter().enumerate() {
        let expected = TitleIdentity::of(want);
        let got = resolve_scanned_title(&scan, idx, Some(&expected))
            .expect("an unchanged re-scan must resolve every title");
        assert_eq!(got.playlist, want.playlist, "at index {idx}");
    }
}

/// An explicit `-t N` on a disc scans exactly once, so there is no earlier
/// list to disagree with and the index is the only reference there is. That
/// path must keep working — but the range rule still applies.
#[test]
fn with_no_earlier_scan_the_index_is_still_range_checked() {
    let scan = first_scan();
    assert_eq!(
        resolve_scanned_title(&scan, 3, None)
            .expect("no expectation recorded → rip what the index names")
            .playlist,
        "00803.mpls"
    );
    assert!(
        resolve_scanned_title(&scan, 4, None).is_err(),
        "one past the end is still out of range"
    );
    let expected = TitleIdentity::of(&scan[3]);
    assert!(
        resolve_scanned_title(&scan[..2], 3, Some(&expected)).is_err(),
        "a shortened list that drops the index off the end still fails"
    );
}

/// The constraint that shapes the identity: duplicate playlists with
/// identical duration and size are LEGITIMATE, so neither field can tell
/// two titles apart. Identity has to come from the playlist and the sectors.
#[test]
fn identity_is_not_duration_or_size() {
    let a = title(800, 1000);
    let b = title(801, 5000);
    assert_eq!(a.duration_secs, b.duration_secs);
    assert_eq!(a.size_bytes, b.size_bytes);
    assert_ne!(
        TitleIdentity::of(&a),
        TitleIdentity::of(&b),
        "two titles that differ only by playlist and sectors must not share an identity"
    );
}

/// Even same-named playlists are separated, as long as they read different
/// sectors — and if they read the same sectors from the same playlist, the
/// two rips are byte-identical, so there is nothing to confuse.
#[test]
fn same_playlist_name_over_different_sectors_is_a_different_title() {
    let mut a = title(800, 1000);
    let mut b = title(800, 9000);
    a.playlist = "FEATURE".into();
    b.playlist = "FEATURE".into();
    assert_ne!(TitleIdentity::of(&a), TitleIdentity::of(&b));
}

#[test]
fn every_expanded_job_looks_up_the_identity_of_its_own_title() {
    let scan = first_scan();
    let identities: Vec<TitleIdentity> = scan.iter().map(TitleIdentity::of).collect();
    let nums = disc_title_nums(true, &[], identities.len());
    assert_eq!(nums, vec![1, 2, 3, 4]);
    for num in nums {
        let idx = num - 1; // what `build_jobs` stores
        let got = job_identity(&identities, Some(idx)).expect("every job has an identity");
        assert_eq!(*got, TitleIdentity::of(&scan[idx]), "job for -t {num}");
        // And it is the identity that lets THAT title through its own re-scan.
        resolve_scanned_title(&scan, idx, Some(got)).expect("stable disc");
    }
    assert!(
        job_identity(&[], Some(0)).is_none(),
        "no upfront scan → nothing to verify against"
    );
}

/// The mismatch message is the whole user-visible surface of this fix: it
/// must read as a sentence, never as the raw `error.title_changed` key that
/// the pinned i18n tag does not ship yet.
#[test]
fn the_mismatch_message_is_readable_and_carries_no_terminal_escapes() {
    let mut hostile = title(801, 5000);
    // The playlist name is on-disc metadata: three bytes are enough for
    // ESC c, a full terminal reset.
    hostile.playlist = "\u{1b}c\n801".into();
    let msg = title_changed_message(
        2,
        &TitleIdentity::of(&hostile),
        &TitleIdentity::of(&title(802, 9000)),
    );
    assert!(
        !msg.starts_with("error."),
        "the raw key path reached the user: {msg:?}"
    );
    assert!(
        !msg.contains('\u{1b}'),
        "ESC survived into terminal output: {msg:?}"
    );
    assert!(msg.contains("00802.mpls"), "got {msg:?}");
}
