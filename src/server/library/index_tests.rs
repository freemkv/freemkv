use super::*;

#[test]
fn titles_normalise_the_way_the_prototype_did() {
    assert_eq!(normalise_title("Fast & Furious (2009)"), "fastandfurious");
    assert_eq!(normalise_title("FastAndFurious"), "fastandfurious");
    assert_eq!(normalise_title("Alien: Romulus (2024)"), "alienromulus");
    assert_eq!(
        normalise_title("2001 A Space Odyssey (1968)"),
        "2001aspaceodyssey"
    );
    // Only a four-digit year in brackets goes; other brackets keep their text.
    assert_eq!(
        normalise_title("Blade Runner (Final Cut)"),
        "bladerunnerfinalcut"
    );
    assert_eq!(normalise_title("Se7en (1995) (4K)"), "se7en4k");
    assert_eq!(normalise_title("Amélie (2001)"), "amélie");
    assert_ne!(
        normalise_title("千と千尋の神隠し"),
        normalise_title("もののけ姫")
    );
    assert_ne!(normalise_title("!!!"), normalise_title("???"));
    assert_eq!(normalise_title("(12345)"), "12345");
    assert_eq!(normalise_title(""), "");
}

#[test]
fn segments_keep_brackets_and_lose_separators() {
    assert_eq!(safe_segment("New Film (2020)"), "New Film (2020)");
    assert_eq!(safe_segment("../../etc/passwd"), "etcpasswd");
    assert_eq!(safe_segment("a/b\\c:d"), "abcd");
    assert_eq!(safe_segment(" .. "), "untitled");
    assert_eq!(safe_segment(""), "untitled");
}

fn touch(p: &Path) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, b"x").unwrap();
}

fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let t = tempfile::tempdir().unwrap();
    let lib = t.path().join("movies");
    let isos = t.path().join("isos");
    touch(&lib.join("Heat (1995)/Heat (1995).mkv"));
    touch(&lib.join("Heat (1995)/Heat (1995).mkv.partial"));
    touch(&lib.join("Only Here (2001)/Only Here (2001).mkv"));
    touch(&lib.join("Twice (2010)/Twice (2010).mkv"));
    touch(&lib.join("Twice (2010)/Twice (2010) - extras.mkv"));
    touch(&lib.join("Loose.mkv"));
    touch(&isos.join("Heat (1995).iso"));
    touch(&isos.join("New Film (2020).iso"));
    touch(&isos.join("Dup (1999).iso"));
    touch(&isos.join("DUP.iso"));
    touch(&isos.join("dvd/Old Disc (1988).iso"));
    touch(&isos.join("bd/Heat.iso"));
    touch(&isos.join("notes.txt"));
    touch(&isos.join("Twice (2010).iso"));
    touch(&lib.join("Pair (2000)/a.mkv"));
    touch(&lib.join("Pair (2000)/b.mkv"));
    touch(&isos.join("Pair (2000).iso"));
    (t, lib, isos)
}

fn by_title<'a>(rows: &'a [Row], title: &str) -> &'a Row {
    rows.iter()
        .find(|r| r.title == title)
        .unwrap_or_else(|| panic!("no row {title}: {rows:#?}"))
}

#[test]
fn listings_skip_partials_and_gate_subfolders() {
    let (_t, lib, isos) = fixture();
    let m = list_mkvs(&lib);
    assert!(!m.incomplete);
    assert_eq!(m.files.len(), 7, "the .partial is not an MKV");
    assert!(m.files.iter().any(|f| f.title == "Loose"));
    let top = list_isos(&isos, false);
    assert_eq!(top.files.len(), 6);
    let all = list_isos(&isos, true);
    assert_eq!(all.files.len(), 8);
    assert!(all.files.iter().any(|f| f.title == "Old Disc (1988)"));
}

#[test]
fn cross_list_classifies_every_kind() {
    let (_t, lib, isos) = fixture();
    let mkvs = list_mkvs(&lib).files;
    let rows = classify(&lib, &mkvs, &list_isos(&isos, false).files, &HashMap::new());

    let heat = by_title(&rows, "Heat (1995)");
    assert_eq!(heat.kind, RowKind::Remux);
    assert_eq!(heat.target, heat.mkv);
    assert!(heat.iso.as_ref().unwrap().ends_with("Heat (1995).iso"));

    let new = by_title(&rows, "New Film (2020)");
    assert_eq!(new.kind, RowKind::IsoOnly);
    assert_eq!(
        new.target.as_deref(),
        Some(lib.join("New Film (2020)/New Film (2020).mkv").as_path())
    );
    assert!(new.remuxable());

    let only = by_title(&rows, "Only Here (2001)");
    assert_eq!(only.kind, RowKind::MkvOnly);
    assert!(only.target.is_none() && !only.remuxable());

    let dup = rows.iter().find(|r| r.key == "dup").unwrap();
    assert_eq!(dup.kind, RowKind::Ambiguous);
    assert_eq!(dup.note, Some(RowNote::SeveralIsos { count: 2 }));
    assert!(dup.iso.is_none() && dup.target.is_none());

    let twice = rows.iter().find(|r| r.key == "twice").unwrap();
    assert_eq!(
        twice.kind,
        RowKind::Remux,
        "the extras MKV is not the feature"
    );
    assert!(
        twice
            .mkv
            .as_ref()
            .unwrap()
            .ends_with("Twice (2010)/Twice (2010).mkv")
    );
    let pair = rows.iter().find(|r| r.key == "pair").unwrap();
    assert_eq!(pair.kind, RowKind::Ambiguous);
    assert_eq!(pair.note, Some(RowNote::SeveralMkvs { count: 2 }));
    assert!(pair.target.is_none());
    assert_eq!(by_title(&rows, "Loose").kind, RowKind::MkvOnly);
}

#[test]
fn remakes_with_different_years_are_not_paired() {
    let t = tempfile::tempdir().unwrap();
    let lib = t.path().join("movies");
    let isos = t.path().join("isos");
    touch(&lib.join("King Kong (1933)/King Kong (1933).mkv"));
    touch(&isos.join("King Kong (2005).iso"));
    touch(&lib.join("七人の侍/七人の侍.mkv"));
    touch(&isos.join("羅生門.iso"));
    let rows = classify(
        &lib,
        &list_mkvs(&lib).files,
        &list_isos(&isos, false).files,
        &HashMap::new(),
    );
    assert_eq!(by_title(&rows, "King Kong (1933)").kind, RowKind::MkvOnly);
    assert_eq!(by_title(&rows, "King Kong (2005)").kind, RowKind::IsoOnly);
    assert_eq!(by_title(&rows, "七人の侍").kind, RowKind::MkvOnly);
    assert_eq!(by_title(&rows, "羅生門").kind, RowKind::IsoOnly);
}

#[test]
fn subfolder_isos_can_make_a_match_ambiguous() {
    let (_t, lib, isos) = fixture();
    let mkvs = list_mkvs(&lib).files;
    let rows = classify(&lib, &mkvs, &list_isos(&isos, true).files, &HashMap::new());
    let heat = rows.iter().find(|r| r.key == "heat").unwrap();
    assert_eq!(
        heat.kind,
        RowKind::Ambiguous,
        "bd/Heat.iso is a second candidate"
    );
    assert_eq!(by_title(&rows, "Old Disc (1988)").kind, RowKind::IsoOnly);
}

#[test]
fn a_recorded_link_beats_title_matching() {
    let (_t, lib, isos) = fixture();
    let mkvs = list_mkvs(&lib).files;
    let all = list_isos(&isos, true).files;
    let heat_mkv = lib.join("Heat (1995)/Heat (1995).mkv");
    let links = HashMap::from([(heat_mkv.clone(), isos.join("bd/Heat.iso"))]);
    let rows = classify(&lib, &mkvs, &all, &links);
    let linked: Vec<_> = rows.iter().filter(|r| r.linked).collect();
    assert_eq!(linked.len(), 1);
    assert_eq!(linked[0].kind, RowKind::Remux);
    assert_eq!(linked[0].target.as_ref(), Some(&heat_mkv));
    // The other Heat ISO no longer competes; it stands alone as ISO-only.
    let rest = rows.iter().find(|r| r.key == "heat" && !r.linked).unwrap();
    assert_eq!(rest.kind, RowKind::IsoOnly);
    assert!(rest.iso.as_ref().unwrap().ends_with("Heat (1995).iso"));
}

#[test]
fn a_link_to_a_vanished_iso_falls_back_to_matching() {
    let (_t, lib, isos) = fixture();
    let mkvs = list_mkvs(&lib).files;
    let heat_mkv = lib.join("Heat (1995)/Heat (1995).mkv");
    let links = HashMap::from([(heat_mkv, isos.join("gone.iso"))]);
    let rows = classify(&lib, &mkvs, &list_isos(&isos, false).files, &links);
    assert_eq!(by_title(&rows, "Heat (1995)").kind, RowKind::Remux);
    assert!(!by_title(&rows, "Heat (1995)").linked);
}

#[test]
fn an_unreadable_or_missing_folder_is_an_incomplete_listing() {
    let t = tempfile::tempdir().unwrap();
    let gone = t.path().join("not-mounted");
    assert!(list_mkvs(&gone).incomplete);
    assert!(list_isos(&gone, true).incomplete);
    assert!(list_mkvs(&gone).files.is_empty());
}

#[cfg(unix)]
#[test]
fn one_unreadable_subfolder_marks_the_listing_incomplete() {
    use std::os::unix::fs::PermissionsExt as _;
    let t = tempfile::tempdir().unwrap();
    let lib = t.path().join("movies");
    touch(&lib.join("A/A.mkv"));
    touch(&lib.join("B/B.mkv"));
    let b = lib.join("B");
    std::fs::set_permissions(&b, std::fs::Permissions::from_mode(0o0)).unwrap();
    let m = list_mkvs(&lib);
    std::fs::set_permissions(&b, std::fs::Permissions::from_mode(0o755)).unwrap();
    if std::fs::read_dir(&b).is_ok() && m.files.len() == 2 {
        return; // root reads anything
    }
    assert!(m.incomplete);
    assert_eq!(m.files.len(), 1);
}
