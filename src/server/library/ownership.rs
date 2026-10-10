//! Explicit legacy ownership uses server snapshots, never client-supplied identities.

use super::replacement::{OwnedOutput, Ownership, Replacement};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Confirmation {
    pub preview_token: String,
    pub selected_candidates: Vec<usize>,
    pub confirm_ownership: bool,
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn unrelated_symlinks_and_both_aliases_are_omitted_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.iso");
        let safe = dir.path().join("safe.mkv");
        let alias = dir.path().join("alias.mkv");
        let original = dir.path().join("original.mkv");
        let symlink = dir.path().join("symlink.mkv");
        std::fs::write(&source, b"iso").unwrap();
        std::fs::write(&safe, b"safe").unwrap();
        std::fs::write(&original, b"old").unwrap();
        std::fs::hard_link(&original, &alias).unwrap();
        std::os::unix::fs::symlink(&safe, &symlink).unwrap();
        let preview = Previews::default()
            .create(
                &source,
                0,
                vec![dir.path().to_owned()],
                vec![safe.clone(), original, alias, symlink],
                &HashMap::new(),
            )
            .unwrap();
        assert_eq!(preview.candidates.len(), 1);
        assert_eq!(preview.candidates[0].path, safe);
        assert_eq!(preview.omitted_candidates, 3);
    }

    #[test]
    fn alias_of_another_sources_linked_output_is_not_offered() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.iso");
        let owned = dir.path().join("owned.mkv");
        let alias = dir.path().join("alias.mkv");
        std::fs::write(&source, b"iso").unwrap();
        std::fs::write(&owned, b"other").unwrap();
        std::fs::hard_link(&owned, &alias).unwrap();
        let links = HashMap::from([(owned, dir.path().join("other.iso"))]);
        let preview = Previews::default()
            .create(&source, 0, vec![dir.path().to_owned()], vec![alias], &links)
            .unwrap();
        assert!(preview.candidates.is_empty());
        assert_eq!(preview.omitted_candidates, 1);
    }

    #[test]
    fn candidate_limit_precedes_any_source_or_output_io() {
        let error = Previews::default()
            .create(
                Path::new("missing.iso"),
                0,
                vec![],
                vec![PathBuf::from("missing.mkv"); 4097],
                &HashMap::new(),
            )
            .err()
            .unwrap();
        assert!(error.to_string().contains("too many indexed"));
    }

    #[test]
    fn admission_rejects_stale_forged_conflicting_and_unaffirmed_requests() {
        for mutation in [
            "source",
            "file",
            "revision",
            "roots",
            "owner",
            "duplicate",
            "unknown",
            "affirmation",
            "token",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let source = dir.path().join("source.iso");
            let output = dir.path().join("old.mkv");
            std::fs::write(&source, b"source").unwrap();
            std::fs::write(&output, b"old").unwrap();
            let mut roots = vec![dir.path().to_owned()];
            let mut links = HashMap::new();
            let mut previews = Previews::default();
            let preview = previews
                .create(&source, 1, roots.clone(), vec![output.clone()], &links)
                .unwrap();
            let mut request = Confirmation {
                preview_token: preview.preview_token,
                selected_candidates: vec![0],
                confirm_ownership: true,
            };
            let mut revision = 1;
            match mutation {
                "source" => std::fs::write(&source, b"changed source").unwrap(),
                "file" => std::fs::write(&output, b"changed file").unwrap(),
                "revision" => revision = 2,
                "roots" => roots.clear(),
                "owner" => {
                    links.insert(output.clone(), dir.path().join("other.iso"));
                }
                "duplicate" => request.selected_candidates.push(0),
                "unknown" => request.selected_candidates = vec![99],
                "affirmation" => request.confirm_ownership = false,
                "token" => request.preview_token = "forged".into(),
                _ => unreachable!(),
            }
            assert!(
                previews
                    .consume(&source, revision, &roots, request, &links)
                    .is_err(),
                "{mutation}"
            );
            assert!(output.exists());
        }
    }

    #[test]
    fn successful_authorization_is_durable_distinct_and_single_use() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.iso");
        let output = dir.path().join("old.mkv");
        std::fs::write(&source, b"source").unwrap();
        std::fs::write(&output, b"old").unwrap();
        let roots = vec![dir.path().to_owned()];
        let mut previews = Previews::default();
        let preview = previews
            .create(&source, 0, roots.clone(), vec![output], &HashMap::new())
            .unwrap();
        let request = || Confirmation {
            preview_token: preview.preview_token.clone(),
            selected_candidates: vec![0],
            confirm_ownership: true,
        };
        let authorized = previews
            .consume(&source, 0, &roots, request(), &HashMap::new())
            .unwrap();
        let recovered: Replacement =
            serde_json::from_slice(&serde_json::to_vec(&authorized).unwrap()).unwrap();
        recovered.verify_admission(&source).unwrap();
        assert_eq!(recovered.old_outputs[0].ownership, Ownership::UserConfirmed);
        assert!(
            previews
                .consume(&source, 0, &roots, request(), &HashMap::new())
                .is_err()
        );
        assert!(
            serde_json::from_value::<Confirmation>(serde_json::json!({
                "preview_token": "x", "selected_candidates": [], "confirm_ownership": true,
                "identity": {"forged": true}
            }))
            .is_err()
        );
    }
}

#[derive(Serialize)]
pub(super) struct Candidate {
    pub id: usize,
    pub path: PathBuf,
    pub size_bytes: u64,
}

#[derive(Serialize)]
pub(super) struct Preview {
    pub preview_token: String,
    pub owned_outputs: Vec<PathBuf>,
    pub candidates: Vec<Candidate>,
    pub omitted_candidates: usize,
}

struct Frozen {
    created: Instant,
    source: PathBuf,
    revision: u64,
    roots: Vec<PathBuf>,
    replacement: Replacement,
    candidates: Vec<OwnedOutput>,
}

#[derive(Default)]
pub(super) struct Previews(HashMap<String, Frozen>);

fn stale(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::AlreadyExists, message)
}

pub(super) fn validate_output(roots: &[PathBuf], path: &Path) -> io::Result<()> {
    let root = roots
        .iter()
        .find(|root| path.starts_with(root))
        .ok_or_else(|| stale("confirmed output is outside the configured library roots"))?;
    if !path.is_absolute()
        || path == root
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        || !path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("mkv"))
    {
        return Err(stale("invalid confirmed output path"));
    }
    let mut at = root.clone();
    if std::fs::symlink_metadata(&at)?.file_type().is_symlink() {
        return Err(stale("symlinked library root is not permitted"));
    }
    for component in path
        .strip_prefix(root)
        .map_err(io::Error::other)?
        .components()
    {
        at.push(component);
        if std::fs::symlink_metadata(&at)?.file_type().is_symlink() {
            return Err(stale("symlinked confirmed output is not permitted"));
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if std::fs::symlink_metadata(path)?.nlink() != 1 {
            return Err(stale("hard-linked confirmed output is not permitted"));
        }
    }
    if !path.canonicalize()?.starts_with(root.canonicalize()?) {
        return Err(stale("confirmed output escapes the library root"));
    }
    Ok(())
}

impl Previews {
    pub fn install(&mut self, prepared: Self) -> io::Result<()> {
        self.0
            .retain(|_, preview| preview.created.elapsed() < Duration::from_secs(600));
        if self.0.len() + prepared.0.len() > 32 {
            return Err(stale(
                "too many open ownership previews; close dialogs and retry later",
            ));
        }
        self.0.extend(prepared.0);
        Ok(())
    }

    pub fn create(
        &mut self,
        source: &Path,
        revision: u64,
        roots: Vec<PathBuf>,
        paths: Vec<PathBuf>,
        links: &HashMap<PathBuf, PathBuf>,
    ) -> io::Result<Preview> {
        if paths.len() > 4096 || links.len() > 4096 {
            return Err(stale("too many indexed outputs for ownership preview"));
        }
        let mut replacement = Replacement::capture(source, links)?;
        replacement.bind_roots(&roots)?;
        let source_identity = super::replacement::FileIdentity::read(source)?;
        for output in &replacement.old_outputs {
            validate_output(&roots, &output.path)?;
        }
        let mut inspected = Vec::new();
        let mut omitted_candidates = 0;
        let mut seen = HashSet::new();
        for path in paths.into_iter().chain(links.keys().cloned()) {
            if !seen.insert(path.clone()) {
                continue;
            }
            let output =
                validate_output(&roots, &path).and_then(|()| Replacement::candidate(&path));
            match output {
                Ok(output) => inspected.push(output),
                Err(_) if !links.contains_key(&path) => omitted_candidates += 1,
                Err(_) => {}
            }
        }
        let mut candidates: Vec<_> = inspected
            .iter()
            .filter(|output| {
                if links.contains_key(&output.path) {
                    return false;
                }
                let aliased = output.identity.same_file(&source_identity)
                    || inspected.iter().any(|other| {
                        other.path != output.path && other.identity.same_file(&output.identity)
                    });
                if aliased {
                    omitted_candidates += 1;
                }
                !aliased
            })
            .cloned()
            .collect();
        candidates.sort_by(|a, b| a.path.cmp(&b.path));
        replacement.verify_admission(source)?;
        self.0
            .retain(|_, preview| preview.created.elapsed() < Duration::from_secs(600));
        if self.0.len() >= 32 {
            return Err(stale(
                "too many open ownership previews; close dialogs and retry later",
            ));
        }
        static SEQUENCE: AtomicU64 = AtomicU64::new(1);
        let token = format!(
            "{:x}-{:x}-{:x}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(io::Error::other)?
                .as_nanos(),
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        let view = Preview {
            preview_token: token.clone(),
            omitted_candidates,
            owned_outputs: replacement
                .old_outputs
                .iter()
                .map(|o| o.path.clone())
                .collect(),
            candidates: candidates
                .iter()
                .enumerate()
                .map(|(id, output)| Candidate {
                    id,
                    path: output.path.clone(),
                    size_bytes: Replacement::output_size(output),
                })
                .collect(),
        };
        self.0.insert(
            token,
            Frozen {
                created: Instant::now(),
                source: source.to_path_buf(),
                revision,
                roots,
                replacement,
                candidates,
            },
        );
        Ok(view)
    }

    pub fn consume(
        &mut self,
        source: &Path,
        revision: u64,
        roots: &[PathBuf],
        confirmation: Confirmation,
        links: &HashMap<PathBuf, PathBuf>,
    ) -> io::Result<Replacement> {
        let frozen = self.0.remove(&confirmation.preview_token).ok_or_else(|| {
            stale("ownership preview expired or was already used; reopen Change match")
        })?;
        if frozen.created.elapsed() >= Duration::from_secs(600)
            || frozen.source != source
            || frozen.revision != revision
            || frozen.roots != roots
        {
            return Err(stale("ownership preview is stale; reopen Change match"));
        }
        if !confirmation.selected_candidates.is_empty() && !confirmation.confirm_ownership {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "explicit legacy ownership confirmation is required",
            ));
        }
        let mut replacement = frozen.replacement;
        let mut selected = HashSet::new();
        for id in confirmation.selected_candidates {
            if !selected.insert(id) {
                return Err(stale("duplicate confirmed candidate"));
            }
            let output = frozen
                .candidates
                .get(id)
                .ok_or_else(|| stale("unknown confirmed candidate"))?;
            if links.contains_key(&output.path) {
                return Err(stale("legacy output ownership changed"));
            }
            validate_output(roots, &output.path)?;
            replacement.confirm(output.clone())?;
        }
        for output in &replacement.old_outputs {
            validate_output(roots, &output.path)?;
            if output.ownership == Ownership::RecordedLink
                && links.get(&output.path).map(PathBuf::as_path) != Some(source)
            {
                return Err(stale("recorded source ownership changed"));
            }
        }
        replacement.verify_admission(source)?;
        Ok(replacement)
    }
}
