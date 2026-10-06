use super::{build_jobs, disc_title_nums};
use crate::output::Output;
use libfreemkv::parse_url;

#[test]
fn an_empty_scanned_title_list_still_builds_a_job() {
    let out = Output::new(false, true);
    let dest = "mkv:///tmp/fmkv-empty-scan.mkv";
    let parsed = parse_url(dest);
    let jobs = build_jobs(&Some(vec![]), false, &[], false, dest, &parsed, &out)
        .expect("an empty title list must not fail job building");
    assert_eq!(
        jobs.len(),
        1,
        "an empty scan produced {} jobs — a zero-job rip exits 0 having \
             written nothing",
        jobs.len()
    );
    assert_eq!(jobs[0].0, None, "no title was selected, so no index");
}

#[test]
fn one_title_into_a_directory_is_still_named_per_title() {
    let out = Output::new(false, true);
    let dir = super::tests::temp_path("one-into-dir");
    let dest = format!("mkv://{}/", dir.display());
    let parsed = parse_url(&dest);

    let titles = Some(vec![
        libfreemkv::DiscTitle::empty(),
        libfreemkv::DiscTitle::empty(),
    ]);
    let jobs = build_jobs(&titles, false, &[1], true, &dest, &parsed, &out)
        .expect("a directory dest accepts a single title");
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].0, Some(0), "-t 1 is the first title, 0-based");
    assert!(
        jobs[0].1.ends_with("_t1.mkv"),
        "a directory dest must still name the file per title, got {}",
        jobs[0].1
    );
    assert_ne!(
        jobs[0].1, dest,
        "the directory itself is not the output file"
    );

    // The same title against a single FILE dest goes straight to that file.
    let file = "mkv:///tmp/fmkv-one.mkv";
    let pf = parse_url(file);
    let jobs = build_jobs(&titles, false, &[1], false, file, &pf, &out)
        .expect("a file dest accepts a single title");
    assert_eq!(jobs, vec![(Some(0), file.to_string())]);

    let _ = std::fs::remove_dir_all(&dir);
}

// A disc source's one title into a directory is named per title, as an image's is.
#[test]
fn one_disc_title_into_a_directory_is_named_per_title() {
    let out = Output::new(false, true);
    let dir = super::tests::temp_path("disc-one-into-dir");
    let dest = format!("mkv://{}/", dir.display());
    let parsed = parse_url(&dest);
    let jobs = build_jobs(&None, true, &[2], true, &dest, &parsed, &out)
        .expect("a directory dest accepts a single title");
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].0, Some(1), "-t 2 is the second title, 0-based");
    assert!(jobs[0].1.ends_with("disc_t2.mkv"), "got {}", jobs[0].1);
    let _ = std::fs::remove_dir_all(&dir);
}

// Several titles of an image onto a single-file name: one file per title beside it
// (`-t 3 -t 4 mkv://d/sel.mkv` -> `d/sel_t3.mkv`, `d/sel_t4.mkv`).
#[test]
fn several_image_titles_fan_out_beside_a_single_file_name() {
    let out = Output::new(false, true);
    let file = super::tests::temp_path("img-multi-fanout").join("sel.mkv");
    let dest = format!("mkv://{}", file.display());
    let parsed = parse_url(&dest);
    let titles = Some(vec![libfreemkv::DiscTitle::empty(); 4]);
    let jobs = build_jobs(&titles, false, &[3, 4], false, &dest, &parsed, &out);
    let made_dir = file.is_dir();
    let _ = std::fs::remove_dir_all(file.parent().unwrap());
    let jobs = jobs.expect("several titles onto a file name must fan out, not refuse");
    assert!(!made_dir, "the file name is not turned into a directory");
    let beside = |n: u32| {
        format!(
            "mkv://{}",
            file.with_file_name(format!("sel_t{n}.mkv")).display()
        )
    };
    assert_eq!(jobs, vec![(Some(2), beside(3)), (Some(3), beside(4))]);
}

// A destination directory that cannot be read cannot be shown empty.
#[test]
fn an_unreadable_dir_dest_is_refused_not_read_as_empty() {
    let base = super::tests::temp_path("unreadable-dest");
    std::fs::create_dir_all(&base).unwrap();
    let file = base.join("plain-file");
    std::fs::write(&file, b"x").unwrap();
    // A path under a plain file is unreadable on Unix (ENOTDIR); Windows reports it as
    // not found, which is the missing-directory case below.
    #[cfg(unix)]
    assert!(super::validate_dir_dest(&file.join("sub"), "dir://x", false).is_err());
    // A missing directory is still just empty.
    assert!(super::validate_dir_dest(&base.join("missing"), "dir://x", false).is_ok());
    let _ = std::fs::remove_dir_all(&base);
}

// `-t all` of a disc onto one file name writes one file per title beside it.
#[test]
fn an_expanded_disc_selection_fans_out_beside_a_single_file_name() {
    let out = Output::new(false, true);
    let dest = "mkv:///tmp/fmkv-t-all.mkv";
    let parsed = parse_url(dest);
    let nums = disc_title_nums(true, &[], 12);
    let jobs = build_jobs(&None, true, &nums, false, dest, &parsed, &out)
        .expect("twelve titles fan out, not refuse");
    let beside = |n: u32| {
        let file = std::path::Path::new("/tmp").join(format!("fmkv-t-all_t{n}.mkv"));
        format!("mkv://{}", file.display())
    };
    assert_eq!(jobs.len(), 12);
    assert_eq!(jobs[0], (Some(0), beside(1)));
    assert_eq!(jobs[11], (Some(11), beside(12)));
}
