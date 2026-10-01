//! The cross-list: library MKVs and source ISOs matched into rows.
//!
//! MKVs live as `Title/Title.mkv` under the library folder; ISOs sit flat in
//! the ISO folder (plus one level of subfolders such as `dvd/` or `bd/` when
//! enabled). A recorded rip-time link wins; otherwise the two sides meet on
//! [`normalise_title`].

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

/// `"Fast & Furious (2009)"` and `"FastAndFurious"` both become `"fastandfurious"`:
/// lowercase, `&` read as "and", any `(YYYY)` dropped, then letters and digits only
/// (any script). A title with none keeps its trimmed lowercase text, so it never
/// shares a key with every other such title.
pub fn normalise_title(s: &str) -> String {
    let lower = s.to_lowercase().replace('&', "and");
    let mut out = String::with_capacity(lower.len());
    let chars: Vec<char> = lower.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let year = chars.len() - i >= 6
            && chars[i] == '('
            && chars[i + 1..i + 5].iter().all(|c| c.is_ascii_digit())
            && chars[i + 5] == ')';
        if year {
            i += 6;
            continue;
        }
        if chars[i].is_alphanumeric() {
            out.push(chars[i]);
        }
        i += 1;
    }
    if out.is_empty() {
        return lower.trim().to_string();
    }
    out
}

/// The first `(YYYY)` in a title, which normalising drops.
fn title_year(s: &str) -> Option<u32> {
    let b = s.as_bytes();
    (0..b.len().saturating_sub(5)).find_map(|i| {
        let w = &b[i..i + 6];
        (w[0] == b'(' && w[5] == b')' && w[1..5].iter().all(u8::is_ascii_digit))
            .then(|| s[i + 1..i + 5].parse().ok())
            .flatten()
    })
}

/// One MKV found under the library folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MkvFile {
    pub path: PathBuf,
    /// The title folder (first path component), or the file stem for a loose file.
    pub title: String,
}

/// One ISO found in the ISO folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IsoFile {
    pub path: PathBuf,
    /// The file stem, which is how autorip names a kept ISO (`Title (Year).iso`).
    pub title: String,
}

/// What a listing saw, plus whether any part of it could not be read.
#[derive(Debug)]
pub struct Listing<T> {
    pub files: Vec<T>,
    pub incomplete: bool,
}

impl<T> Default for Listing<T> {
    fn default() -> Self {
        Self {
            files: Vec::new(),
            incomplete: false,
        }
    }
}

fn has_ext(p: &Path, ext: &str) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case(ext))
}

/// Every `.mkv` under `root` (any depth). A `.partial` is never listed.
pub fn list_mkvs(root: &Path) -> Listing<MkvFile> {
    let mut out = Listing::default();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            out.incomplete = true;
            continue;
        };
        for entry in entries {
            let Ok(entry) = entry else {
                out.incomplete = true;
                continue;
            };
            let path = entry.path();
            let Ok(ft) = entry.file_type() else {
                out.incomplete = true;
                continue;
            };
            if ft.is_dir() {
                stack.push(path);
            } else if has_ext(&path, "mkv") {
                let title = mkv_title(root, &path);
                out.files.push(MkvFile { path, title });
            }
        }
    }
    out.files.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

pub(crate) fn mkv_title(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    let mut parts = rel.components();
    let first = parts.next();
    match (first, parts.next()) {
        (Some(dir), Some(_)) => dir.as_os_str().to_string_lossy().into_owned(),
        _ => stem(path),
    }
}

fn stem(p: &Path) -> String {
    p.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The `.iso` files directly in `root`, and with `subfolders` also those one
/// folder down (`dvd/`, `hddvd/`, `bd/`, ...).
pub fn list_isos(root: &Path, subfolders: bool) -> Listing<IsoFile> {
    let mut out = Listing::default();
    let mut dirs = vec![(root.to_path_buf(), 0u8)];
    while let Some((dir, depth)) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            out.incomplete = true;
            continue;
        };
        for entry in entries {
            let Ok(entry) = entry else {
                out.incomplete = true;
                continue;
            };
            let path = entry.path();
            // `metadata` follows a symlinked ISO, which `file_type` would not.
            let Ok(meta) = std::fs::metadata(&path) else {
                out.incomplete = true;
                continue;
            };
            if meta.is_dir() {
                if subfolders && depth == 0 {
                    dirs.push((path, 1));
                }
            } else if has_ext(&path, "iso") {
                let title = stem(&path);
                out.files.push(IsoFile { path, title });
            }
        }
    }
    out.files.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// How a row can be treated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RowKind {
    /// One ISO and one MKV: the MKV can be remuxed in place.
    Remux,
    /// One ISO and no MKV yet: a remux creates `Title/Title.mkv`.
    IsoOnly,
    /// An MKV with no ISO: listed, never touched.
    MkvOnly,
    /// More than one candidate on a side: listed, never touched.
    Ambiguous,
}

/// Why a row is ambiguous.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RowNote {
    SeveralIsos { count: usize },
    SeveralMkvs { count: usize },
}

/// One title in the cross-list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub key: String,
    pub title: String,
    pub kind: RowKind,
    pub note: Option<RowNote>,
    /// The MKV on disk, when there is exactly one.
    pub mkv: Option<PathBuf>,
    /// The ISO, when there is exactly one.
    pub iso: Option<PathBuf>,
    /// Where a remux writes: the MKV for `Remux`, `Title/Title.mkv` for `IsoOnly`.
    pub target: Option<PathBuf>,
    /// The pairing came from a link recorded at rip time.
    pub linked: bool,
}

impl Row {
    pub fn remuxable(&self) -> bool {
        matches!(self.kind, RowKind::Remux | RowKind::IsoOnly)
    }
}

/// `name` made safe as one path segment. Unlike the rip-side sanitisers it keeps
/// brackets, so `Title (Year)` survives as the library names it.
pub fn safe_segment(name: &str) -> String {
    let kept: String = name
        .chars()
        .filter(|c| !c.is_control() && !r#"/\:*?"<>|"#.contains(*c))
        .collect();
    let trimmed = kept.trim().trim_start_matches('.').trim();
    if trimmed.is_empty() {
        "untitled".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Where an ISO-only title's MKV goes: `<library>/<Title>/<Title>.mkv`.
pub fn new_mkv_target(library: &Path, iso_title: &str) -> PathBuf {
    let name = safe_segment(iso_title);
    library.join(&name).join(format!("{name}.mkv"))
}

/// Match `mkvs` and `isos` into rows. `links` maps an MKV path to the ISO it was
/// ripped from; a linked pair is its own row and takes no part in title matching.
pub fn classify(
    library: &Path,
    mkvs: &[MkvFile],
    isos: &[IsoFile],
    links: &HashMap<PathBuf, PathBuf>,
) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut linked_isos = std::collections::HashSet::new();
    let mut unlinked_mkvs = Vec::new();
    let iso_at: HashMap<&Path, &IsoFile> = isos.iter().map(|f| (f.path.as_path(), f)).collect();
    for m in mkvs {
        match links
            .get(&m.path)
            .and_then(|i| iso_at.get(i.as_path()).copied())
        {
            Some(iso) => {
                linked_isos.insert(iso.path.clone());
                rows.push(Row {
                    key: normalise_title(&m.title),
                    title: m.title.clone(),
                    kind: RowKind::Remux,
                    note: None,
                    mkv: Some(m.path.clone()),
                    iso: Some(iso.path.clone()),
                    target: Some(m.path.clone()),
                    linked: true,
                });
            }
            None => unlinked_mkvs.push(m),
        }
    }

    let mut groups: BTreeMap<String, (Vec<&MkvFile>, Vec<&IsoFile>)> = BTreeMap::new();
    for m in unlinked_mkvs {
        groups
            .entry(normalise_title(&m.title))
            .or_default()
            .0
            .push(m);
    }
    for i in isos.iter().filter(|i| !linked_isos.contains(&i.path)) {
        groups
            .entry(normalise_title(&i.title))
            .or_default()
            .1
            .push(i);
    }
    for (key, (ms, is)) in groups {
        // Remakes share a name: same key, different years, different films.
        let years: BTreeSet<u32> = ms
            .iter()
            .map(|m| title_year(&m.title))
            .chain(is.iter().map(|i| title_year(&i.title)))
            .flatten()
            .collect();
        if years.len() < 2 {
            rows.push(group_row(library, key, &ms, &is));
            continue;
        }
        for year in years.iter().map(|y| Some(*y)).chain([None]) {
            let ms: Vec<&MkvFile> = ms
                .iter()
                .copied()
                .filter(|m| title_year(&m.title) == year)
                .collect();
            let is: Vec<&IsoFile> = is
                .iter()
                .copied()
                .filter(|i| title_year(&i.title) == year)
                .collect();
            if !ms.is_empty() || !is.is_empty() {
                let key = year.map_or_else(|| key.clone(), |y| format!("{key}-{y}"));
                rows.push(group_row(library, key, &ms, &is));
            }
        }
    }
    rows.sort_by(|a, b| {
        a.title
            .to_lowercase()
            .cmp(&b.title.to_lowercase())
            .then(a.target.cmp(&b.target))
    });
    rows
}

fn group_row(library: &Path, key: String, ms: &[&MkvFile], is: &[&IsoFile]) -> Row {
    // A title folder may also hold extras; `Title/Title.mkv` is the feature.
    let canonical: Vec<&MkvFile> = ms
        .iter()
        .copied()
        .filter(|m| stem(&m.path) == m.title)
        .collect();
    let ms = if ms.len() > 1 && canonical.len() == 1 {
        &canonical[..]
    } else {
        ms
    };
    let title = ms
        .first()
        .map(|m| m.title.clone())
        .or_else(|| is.first().map(|i| i.title.clone()))
        .unwrap_or_default();
    let mkv = (ms.len() == 1).then(|| ms[0].path.clone());
    let iso = (is.len() == 1).then(|| is[0].path.clone());
    let (kind, note) = if is.len() > 1 {
        (
            RowKind::Ambiguous,
            Some(RowNote::SeveralIsos { count: is.len() }),
        )
    } else if ms.len() > 1 {
        (
            RowKind::Ambiguous,
            Some(RowNote::SeveralMkvs { count: ms.len() }),
        )
    } else if is.len() == 1 && ms.len() == 1 {
        (RowKind::Remux, None)
    } else if is.len() == 1 {
        (RowKind::IsoOnly, None)
    } else {
        (RowKind::MkvOnly, None)
    };
    let target = match kind {
        RowKind::Remux => mkv.clone(),
        RowKind::IsoOnly => Some(new_mkv_target(library, &is[0].title)),
        _ => None,
    };
    Row {
        key,
        title,
        kind,
        note,
        mkv,
        iso,
        target,
        linked: false,
    }
}

#[cfg(test)]
mod tests {
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
}
