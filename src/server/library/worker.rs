//! The remux worker: one job at a time through `freemkv_engine::remux_iso`,
//! yielding the mux slot to any rip, with a watchdog that only ever cancels
//! its own job.

use super::arbiter::Arbiter;
use super::deliver;
use super::owned::Owned;
use super::queue::{Failure, Job, JobNote, JobResult, NewJob, OutputState, StagedFile};
use super::{Dirs, Library, LineKind, Running, transcript};
use crate::server::config::Config;
use crate::server::health::{self, Fault, Problem};
use crate::server::planner::MediaKind;
use freemkv_engine::{Event, Level, Progress, Sink};
use std::io::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

pub(crate) fn shutting_down() -> bool {
    crate::server::SHUTDOWN.load(Ordering::Relaxed)
}

/// The worker loop: waits while a rip holds the slot or the queue is paused.
pub fn run(lib: &Arc<Library>, cfg: &Arc<RwLock<Config>>, arbiter: &Arbiter) {
    tracing::info!("library worker starting");
    let stage = health::remux_stage_dir();
    let mut swept = false;
    while !shutting_down() {
        let snapshot = cfg.read().unwrap_or_else(|e| e.into_inner()).clone();
        let d = super::dirs(&snapshot);
        let output = |dir: &Path| health::preflight("output", dir);
        let now = crate::server::util::epoch_secs();
        swept = swept || sweep_staging_once(lib, &snapshot, stage.as_deref(), now);
        let Some((job, epoch)) = next_job(lib, arbiter, &d, &output, now) else {
            crate::server::sleep_unless_shutdown(Duration::from_secs(1));
            continue;
        };
        run_job(lib, &snapshot, arbiter, epoch, job);
    }
    tracing::info!("library worker stopping");
}

// Sweep the remux staging folder before the first job: no remux has run yet. It waits for a
// queue file that could not be loaded, which would otherwise drop the kept files it adopts.
fn sweep_staging_once(lib: &Library, cfg: &Config, stage: Option<&Path>, now: u64) -> bool {
    if lib.hold_for_unreadable_queue(now) {
        return false;
    }
    if let Some(dir) = stage {
        let library = super::dirs(cfg).library;
        housekeep_staging(lib, dir, &StagingLimits::of(cfg, dir), &library);
    }
    true
}

/// The bounded output-folder check a job must pass before it starts.
pub(crate) type Preflight<'a> = &'a dyn Fn(&Path) -> Result<(), Problem>;

// The next job and the arbiter epoch it started under. Nothing while a rip runs, and
// nothing while a folder the remux needs is unhealthy: the job waits, held, in the queue.
fn next_job(
    lib: &Library,
    arbiter: &Arbiter,
    d: &Dirs,
    preflight: Preflight<'_>,
    now: u64,
) -> Option<(Job, u64)> {
    if arbiter.rip_active() || lib.hold_for_unreadable_queue(now) {
        return None;
    }
    let Some(next) = lib.queue.peek_next() else {
        lib.set_hold(None);
        return None;
    };
    let tv = tv_root_of(d, &next);
    if let Some(hold) = lib.blocked(d, now, tv.is_some()) {
        lib.set_hold(Some(hold));
        return None;
    }
    for folder in std::iter::once(&d.library).chain(tv) {
        if let Err(p) = preflight(folder) {
            tracing::warn!(folder = %p.path.display(), error = %p.detail, "remux held: {}", p.message);
            lib.hold_for(&p, now);
            return None;
        }
    }
    lib.set_hold(None);
    let epoch = arbiter.epoch();
    lib.queue.claim_next().map(|j| (j, epoch))
}

// The separate TV folder, when `job`'s planned outputs file under it.
fn tv_root_of<'a>(d: &'a Dirs, job: &Job) -> Option<&'a std::path::PathBuf> {
    d.tv.as_ref()
        .filter(|tv| job.output_targets().iter().any(|t| t.starts_with(tv)))
}

// A folder check's failure as the error a job ends in, carrying its fault for the retry.
fn problem_error(p: &Problem) -> std::io::Error {
    use std::io::ErrorKind as K;
    let kind = match p.fault {
        Fault::Stale => K::StaleNetworkFileHandle,
        Fault::ReadOnly => K::ReadOnlyFilesystem,
        Fault::Missing => K::NotFound,
        Fault::Denied => K::PermissionDenied,
        Fault::Unresponsive | Fault::Unmounted => K::TimedOut,
        Fault::Io | Fault::Other => K::Other,
    };
    std::io::Error::new(kind, format!("{} {}", folder_sentence(p), p.detail))
}

// The failed folder check as one sentence: its message and the folder.
fn folder_sentence(p: &Problem) -> String {
    crate::strings::fmt_or(
        "server.library.folder_failed",
        "{message} Folder: {path}.",
        &[
            ("message", &p.message),
            ("path", &p.path.display().to_string()),
        ],
    )
}

/// How one job ended, before it is written to the queue.
#[derive(Debug)]
pub(crate) enum Ending {
    Done {
        writing_app: Option<String>,
    },
    /// Cancelled for a rip, a shutdown, or a stall.
    Stopped(JobNote),
    Failed(std::io::Error),
}

fn run_job(lib: &Library, cfg: &Config, arbiter: &Arbiter, epoch: u64, job: Job) {
    let mut job = job;
    let planning = maybe_attach_tv_plan(lib, cfg, &mut job);
    let debug = lib.queue.snapshot().debug_log;
    let log = JobLog::create(&lib.log_path(&job.title));
    lib.stall_cancel.store(false, Ordering::SeqCst);
    lib.set_running(|r| {
        *r = Some(Running {
            job_id: job.id,
            title: job.title.clone(),
            target: job.target.clone(),
            iso: job.iso.clone(),
            phase: "start".into(),
            started_at: crate::server::util::epoch_secs(),
            ..Default::default()
        })
    });
    let sink = JobSink {
        lib,
        job_id: job.id,
        arbiter,
        epoch,
        log,
        debug,
        iso_url: format!("iso://{}", job.iso.display()),
        last_activity: AtomicU64::new(crate::server::util::epoch_secs()),
        term: Mutex::new(Term::default()),
        landing: Mutex::new(None),
    };
    let cmd = match &job.staged {
        Some(kept) => format!(
            "Finishing the remux kept at {}: copying it to {}",
            kept.display(),
            job.target.display()
        ),
        None => command_line(&job),
    };
    sink.emit(LineKind::Cmd, cmd);
    sink.emit(
        LineKind::Out,
        format!("freemkv {}", crate::server::VERSION_LABEL),
    );
    sink.emit(LineKind::Out, String::new());
    let started = Instant::now();
    let done = AtomicBool::new(false);
    let ending = after_tv_planning(planning, || {
        std::thread::scope(|scope| {
            let _ = std::thread::Builder::new()
                .name("library-watchdog".into())
                .spawn_scoped(scope, || watchdog(lib, job.id, &sink, &done));
            let ending = remux(&job, cfg, &sink);
            done.store(true, Ordering::SeqCst);
            ending
        })
    });
    sink.close_open_line();
    finish(lib, &job, ending, started.elapsed(), &sink);
    lib.set_running(|r| *r = None);
}

// The job as the CLI command it runs: its ISO and the file it writes, or the folder its
// files share; a Change match's queue key names no file, so it is never shown.
fn command_line(job: &Job) -> String {
    let iso = format!("$ freemkv iso://{}", job.iso.display());
    let targets = job.output_targets();
    let dest = match targets.as_slice() {
        _ if job.retire && job.plan.is_none() => None,
        [one] => Some(one.clone()),
        [first, rest @ ..] => first
            .ancestors()
            .skip(1)
            .find(|dir| rest.iter().all(|t| t.starts_with(dir)))
            .map(Path::to_path_buf),
        [] => None,
    };
    match dest {
        Some(dest) => format!("{iso} mkv://{}", dest.display()),
        None => iso,
    }
}

/// Freeze automatic title selection before execution, including single outputs.
/// Existing plans and retained media keep their original selection on restart.
fn maybe_attach_tv_plan(lib: &Library, cfg: &Config, job: &mut Job) -> std::io::Result<()> {
    if job.plan.is_some() || job.staged.is_some() {
        return Ok(());
    }
    let saved = lib.match_store()?.get(&job.iso);
    let now = saved.as_ref().map(|_| super::links::iso_stamp(&job.iso));
    let current = saved
        .as_ref()
        .zip(now)
        .is_some_and(|(s, now)| s.is_for(now));
    let episode = linked_episode(&lib.config_dir, job, job.replace || current)?;
    if let Some((saved, now)) = saved.zip(now) {
        if current {
            let row = episode.then(|| job.target.clone());
            return attach_corrected_plan(lib, cfg, job, &saved.media, row.as_deref());
        }
        // Confirmed for an earlier disc filed at this ISO path: not this disc's identity.
        // At the same size only the time moved, so it may be the same disc: never guess.
        let same_size = saved
            .iso_stamp
            .zip(now)
            .is_some_and(|(a, b)| a.size == b.size);
        if job.retire || same_size {
            return Err(std::io::Error::other(crate::strings::fmt_or(
                "server.library.stale_match",
                "The match saved for {path} was confirmed for an earlier disc filed at that path, so nothing was written. Open Change match again for this disc.",
                &[("path", &job.iso.display().to_string())],
            )));
        }
        tracing::info!(iso = %job.iso.display(), "saved match is for an earlier disc at this path; ignored");
    }
    if !cfg.tv_auto {
        if episode {
            return Err(episode_alone(&job.target));
        }
        return attach_automatic_movie_plan(lib, cfg, job);
    }
    let looked_up = if cfg.tmdb_api_key.is_empty() {
        Ok(None)
    } else {
        crate::server::tmdb::lookup(&lookup_label(job, episode), &cfg.tmdb_api_key)
    };
    attach_from_lookup(lib, cfg, job, looked_up)
}

// Whether the job's row is an episode links.json records for its ISO's split disc; a Change
// match plans the whole disc. Unreadable links fail a job that may overwrite (`strict`): a
// guess could plan a whole-disc movie over an episode.
fn linked_episode(config_dir: &Path, job: &Job, strict: bool) -> std::io::Result<bool> {
    if job.retire {
        return Ok(false);
    }
    let links = match links_for_job(config_dir) {
        Ok(links) => links,
        Err(e) if strict => return Err(e),
        Err(_) => return Ok(false),
    };
    Ok(links
        .get(&job.target)
        .is_some_and(|l| l.episode && l.iso == job.iso))
}

// Every link, or the error a job fails with when links.json cannot be read.
fn links_for_job(
    config_dir: &Path,
) -> std::io::Result<std::collections::HashMap<std::path::PathBuf, super::links::Link>> {
    super::links::read_entries(config_dir).map_err(|e| {
        std::io::Error::new(
            e.kind(),
            crate::strings::fmt_or(
                "server.library.links_unreadable",
                "{file} in the config folder could not be read ({error}), so which files this ISO made is unknown; nothing was written. Fix that file or its folder's permissions, then Retry.",
                &[("file", super::links::LINKS_FILE), ("error", &e.to_string())],
            ),
        )
    })
}

// What the disc is looked up by: an episode row is named after one episode, its ISO after the disc.
fn lookup_label(job: &Job, episode: bool) -> String {
    match job.iso.file_stem() {
        Some(stem) if episode => stem.to_string_lossy().into_owned(),
        _ => job.title.clone(),
    }
}

// An episode row remuxes only the planned output that lands on it, never a whole-disc movie.
fn episode_alone(row: &Path) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        crate::strings::fmt_or(
            "server.library.episode_unplanned",
            "{path} is one episode of a disc that was split into several files, and this remux could not find which of the disc's titles it is, so nothing was written. Use Change match on its ISO to remux the whole disc.",
            &[("path", &row.display().to_string())],
        ),
    )
}

/// The one output of a TV plan that lands on `row`, with its target; `None` if none does.
fn only_the_row(
    mut plan: crate::server::planner::RemuxPlan,
    targets: Vec<std::path::PathBuf>,
    row: &Path,
) -> Option<(crate::server::planner::RemuxPlan, Vec<std::path::PathBuf>)> {
    let at = targets.iter().position(|t| t == row)?;
    let output = plan.outputs.swap_remove(at);
    plan.outputs = vec![output];
    Some((plan, vec![row.to_path_buf()]))
}

fn attach_from_lookup(
    lib: &Library,
    cfg: &Config,
    job: &mut Job,
    looked_up: Result<Option<crate::server::tmdb::TmdbResult>, crate::server::tmdb::LookupFailed>,
) -> std::io::Result<()> {
    let found = looked_up.map_err(|_| {
        std::io::Error::other(crate::strings::get_or(
            "remux.tmdb_unreachable",
            "TMDB could not be reached to plan this disc; retry once it is back",
        ))
    })?;
    let episode = linked_episode(&lib.config_dir, job, job.replace)?;
    let label = lookup_label(job, episode);
    let tv_match = found.filter(|m| {
        m.media_type.is_tv() && crate::server::tmdb::is_confident_match(&label, &m.title, m.year)
    });
    let source = freemkv_engine::ImageSource::from_path(&job.iso);
    let (disc, _reader) = freemkv_engine::scan_image(&source).map_err(planning_scan_error)?;
    // TV as the ripper decides it: a TMDB TV match, or a disc label that names a season.
    let kind = if tv_match.is_some() {
        MediaKind::Tv
    } else {
        MediaKind::Movie
    };
    let tv = [
        disc.volume_id.as_str(),
        disc.meta_title.as_deref().unwrap_or_default(),
        label.as_str(),
    ]
    .into_iter()
    .any(|l| crate::server::ripper::plan_kind(kind, l).is_some());
    if !tv {
        if episode {
            return Err(episode_alone(&job.target));
        }
        return persist_movie_plan(lib, cfg, job, &disc.titles);
    }
    let (season, disc_number) = label_numbers(&disc.volume_id, disc.meta_title.as_deref(), &label);
    let (title, year, tmdb_id) = match tv_match {
        Some(m) => (m.title, m.year, m.tmdb_id),
        None => (crate::server::tmdb::clean_title(&label), 0, 0),
    };
    let media = crate::server::planner::MediaMetadata {
        title,
        year,
        tmdb_id,
        season,
        disc: disc_number,
        kind: Some(MediaKind::Tv),
        episode_start: None,
    };
    let plan = tv_plan_result(crate::server::planner::plan(
        cfg,
        &disc.titles,
        &media,
        &job.target,
    ))?;
    if episode {
        let row = tv_destinations(cfg, &plan)
            .and_then(|(_, targets)| only_the_row(plan, targets, &job.target));
        let Some((plan, targets)) = row else {
            return Err(episode_alone(&job.target));
        };
        return persist_plan(lib, job, plan, Some(targets));
    }
    persist_auto_plan(lib, cfg, job, plan)
}

// Keep the OS error and code: finish() classifies storage faults (and their retry) from them.
fn planning_scan_error(error: libfreemkv::Error) -> std::io::Error {
    std::io::Error::from(error)
}

fn persist_auto_plan(
    lib: &Library,
    cfg: &Config,
    job: &mut Job,
    plan: crate::server::planner::RemuxPlan,
) -> std::io::Result<()> {
    let targets = tv_destinations(cfg, &plan).map(|(_, targets)| targets);
    persist_plan(lib, job, plan, targets)
}

fn persist_plan(
    lib: &Library,
    job: &mut Job,
    plan: crate::server::planner::RemuxPlan,
    targets: Option<Vec<std::path::PathBuf>>,
) -> std::io::Result<()> {
    if !lib
        .queue
        .set_plan_targets(job.id, plan, targets)
        .map_err(|e| plan_not_saved(lib, &e))?
    {
        return Err(std::io::Error::other(
            "job changed while freezing title selection",
        ));
    }
    *job = lib
        .queue
        .snapshot()
        .jobs
        .into_iter()
        .find(|j| j.id == job.id)
        .ok_or_else(|| std::io::Error::other("job disappeared while freezing title selection"))?;
    Ok(())
}

// The queue could not record a plan: name its file, and keep the kind for the storage retry.
fn plan_not_saved(lib: &Library, e: &std::io::Error) -> std::io::Error {
    std::io::Error::new(
        e.kind(),
        crate::strings::fmt_or(
            "server.library.plan_not_saved",
            "the remux plan could not be saved to {path} in the config folder: {error}",
            &[
                ("path", &lib.queue.path().display().to_string()),
                ("error", &e.to_string()),
            ],
        ),
    )
}

fn attach_automatic_movie_plan(lib: &Library, cfg: &Config, job: &mut Job) -> std::io::Result<()> {
    let source = freemkv_engine::ImageSource::from_path(&job.iso);
    let (disc, _reader) = freemkv_engine::scan_image(&source).map_err(planning_scan_error)?;
    persist_movie_plan(lib, cfg, job, &disc.titles)
}

fn persist_movie_plan(
    lib: &Library,
    cfg: &Config,
    job: &mut Job,
    titles: &[libfreemkv::DiscTitle],
) -> std::io::Result<()> {
    let plan = crate::server::planner::plan(
        cfg,
        titles,
        &crate::server::planner::MediaMetadata::default(),
        &job.target,
    );
    persist_auto_plan(lib, cfg, job, tv_plan_result(plan)?)
}

// Season and disc number from the disc's labels, as automatic TV planning reads them.
fn label_numbers(
    volume_id: &str,
    meta_title: Option<&str>,
    title: &str,
) -> (Option<u16>, Option<u16>) {
    let read = |from: fn(&str) -> Option<u16>| {
        from(volume_id)
            .or_else(|| meta_title.and_then(from))
            .or_else(|| from(title))
    };
    (
        read(crate::server::tmdb::season_from_label),
        read(crate::server::tmdb::disc_from_label),
    )
}

// A confirmed TV match that leaves season or disc open takes them from the disc's labels.
fn with_label_numbers(
    media: &crate::server::planner::MediaMetadata,
    volume_id: &str,
    meta_title: Option<&str>,
    title: &str,
) -> crate::server::planner::MediaMetadata {
    let mut media = media.clone();
    if media.kind == Some(crate::server::planner::MediaKind::Tv) {
        let (season, disc) = label_numbers(volume_id, meta_title, title);
        media.season = media.season.or(season);
        media.disc = media.disc.or(disc);
    }
    media
}

// `row`: the episode row a plain Remux was asked for; only its output is planned.
fn attach_corrected_plan(
    lib: &Library,
    cfg: &Config,
    job: &mut Job,
    media: &crate::server::planner::MediaMetadata,
    row: Option<&Path>,
) -> std::io::Result<()> {
    if media.kind.is_none() {
        return Err(std::io::Error::other("saved match has no media type"));
    }
    let source = freemkv_engine::ImageSource::from_path(&job.iso);
    let (disc, _reader) = freemkv_engine::scan_image(&source).map_err(planning_scan_error)?;
    let media = &with_label_numbers(
        media,
        &disc.volume_id,
        disc.meta_title.as_deref(),
        &job.title,
    );
    let (plan, targets) = corrected_plan_for(cfg, &disc.titles, media, row)?;
    if !lib
        .queue
        .set_plan_targets(job.id, plan, Some(targets))
        .map_err(|e| plan_not_saved(lib, &e))?
    {
        return Err(std::io::Error::other(
            "job changed while preparing corrected match",
        ));
    }
    *job = lib
        .queue
        .snapshot()
        .jobs
        .into_iter()
        .find(|j| j.id == job.id)
        .ok_or_else(|| std::io::Error::other("corrected remux job disappeared"))?;
    Ok(())
}

// The corrected plan, or for an episode `row` only the output that lands on it.
fn corrected_plan_for(
    cfg: &Config,
    titles: &[libfreemkv::DiscTitle],
    media: &crate::server::planner::MediaMetadata,
    row: Option<&Path>,
) -> std::io::Result<(crate::server::planner::RemuxPlan, Vec<std::path::PathBuf>)> {
    let (plan, targets) = corrected_plan(cfg, titles, media)?;
    match row {
        None => Ok((plan, targets)),
        Some(row) => only_the_row(plan, targets, row).ok_or_else(|| episode_alone(row)),
    }
}

// The plan and library paths a confirmed match produces from the disc's titles.
fn corrected_plan(
    cfg: &Config,
    titles: &[libfreemkv::DiscTitle],
    media: &crate::server::planner::MediaMetadata,
) -> std::io::Result<(crate::server::planner::RemuxPlan, Vec<std::path::PathBuf>)> {
    let Some(kind) = media.kind else {
        return Err(std::io::Error::other("saved match has no media type"));
    };
    let metadata = Some(crate::server::tmdb::TmdbResult {
        title: media.title.clone(),
        year: media.year,
        tmdb_id: media.tmdb_id,
        media_type: kind,
        poster_url: String::new(),
        overview: String::new(),
    });
    // Named as the ripper names a single output; a later disc takes the mover's `_N`.
    let disc = u32::from(media.disc.unwrap_or(1));
    let name = crate::server::util::disc_variant_name(&media.title, disc);
    let target = std::path::PathBuf::from(crate::server::mover::build_destination(
        cfg,
        &metadata,
        &crate::server::ripper::staged_output_name(&name, "mkv"),
        media.season,
    ));
    let plan = crate::server::planner::plan(cfg, titles, media, &target).map_err(|e| {
        std::io::Error::other(e.with_action(crate::strings::fmt_or(
            "server.library.plan.correction_review",
            "Corrected match needs review: {reason}",
            &[("reason", &e.to_string())],
        )))
    })?;
    let targets = tv_destinations(cfg, &plan)
        .map(|(_, paths)| paths)
        .unwrap_or_else(|| vec![target]);
    Ok((plan, targets))
}

fn tv_plan_result(
    result: Result<crate::server::planner::RemuxPlan, crate::server::planner::PlanError>,
) -> std::io::Result<crate::server::planner::RemuxPlan> {
    result.map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            error.with_action(crate::strings::fmt_or(
                "server.library.plan.review",
                "Title plan needs review before remux: {reason}",
                &[("reason", &error.to_string())],
            )),
        )
    })
}

fn after_tv_planning(planning: std::io::Result<()>, remux: impl FnOnce() -> Ending) -> Ending {
    match planning {
        Ok(()) => remux(),
        Err(error) => Ending::Failed(error),
    }
}

fn tv_metadata(media: &crate::server::planner::MediaMetadata) -> crate::server::tmdb::TmdbResult {
    crate::server::tmdb::TmdbResult {
        title: media.title.clone(),
        year: media.year,
        media_type: MediaKind::Tv,
        tmdb_id: media.tmdb_id,
        poster_url: String::new(),
        overview: String::new(),
    }
}

fn tv_destinations(
    cfg: &Config,
    plan: &crate::server::planner::RemuxPlan,
) -> Option<(std::path::PathBuf, Vec<std::path::PathBuf>)> {
    if plan.media.kind != Some(crate::server::planner::MediaKind::Tv)
        || plan.outputs.is_empty()
        || plan.outputs.iter().any(|o| o.episode.is_none())
    {
        return None;
    }
    let metadata = Some(tv_metadata(&plan.media));
    let outputs: Vec<_> = plan
        .outputs
        .iter()
        .map(|o| crate::server::ripper::staging::Output {
            filename: o.filename.clone(),
            title_index: o.title_index,
            title_identity: None,
            episode: o.episode,
            episode_name: o.episode_name.clone(),
            moved: false,
        })
        .collect();
    let targets = outputs
        .iter()
        .map(|o| {
            let leaf = crate::server::mover::tv_episode_leaf(
                &metadata,
                &outputs,
                &o.filename,
                plan.media.season,
            )
            .unwrap_or_else(|| o.filename.clone());
            std::path::PathBuf::from(crate::server::mover::build_destination(
                cfg,
                &metadata,
                &leaf,
                plan.media.season,
            ))
        })
        .collect();
    Some((
        crate::server::mover::destination_root(cfg, &metadata).into(),
        targets,
    ))
}

fn remux(job: &Job, cfg: &Config, sink: &JobSink<'_>) -> Ending {
    if let Some(ending) = cancelled_ending(sink) {
        return ending;
    }
    if let Some(plan) = &job.plan {
        return remux_planned(job, plan, cfg, sink);
    }
    // Planning freezes a plan for every job before it gets here; only a kept file found at
    // startup arrives without one, and only its copy into the library is left to do.
    let Some(kept) = &job.staged else {
        return Ending::Failed(std::io::Error::other(crate::strings::get_or(
            "server.library.no_title_plan",
            "this remux has no title plan; nothing was written",
        )));
    };
    if let Err(why) = safe_to_create(&super::dirs(cfg), &job.target) {
        return Ending::Failed(std::io::Error::other(why));
    }
    if !job.replace
        && let Some(parent) = job.target.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        return Ending::Failed(e);
    }
    match resume_kept(kept, &job.target, sink) {
        Ok(writing_app) => Ending::Done { writing_app },
        Err(e) => cancelled_ending(sink).unwrap_or(Ending::Failed(e)),
    }
}

/// Execute a frozen plan one output at a time. Planning has already happened
/// before this function is entered; this path never re-runs TMDB or episode
/// selection. Each output gets its own staged artifact and final delivery.
fn remux_planned(
    job: &Job,
    plan: &crate::server::planner::RemuxPlan,
    cfg: &Config,
    sink: &JobSink<'_>,
) -> Ending {
    let keys = crate::server::keysource::key_params(cfg);
    let stage = health::remux_stage_dir();
    remux_planned_with(job, plan, cfg, sink, |request, kept, ordinal| {
        if let Some(kept) = kept {
            resume_kept(kept, &request.target, sink)
        } else if let Some(dir) = &stage {
            execute_staged(request, sink, dir, ordinal, |staged, capture| {
                freemkv_engine::remux_iso(staged, &keys, capture)
            })
        } else {
            freemkv_engine::remux_iso(request, &keys, sink).map(|r| r.writing_app)
        }
    })
}

// One output through the remux staging folder, numbered by job and output.
fn execute_staged(
    request: &freemkv_engine::RemuxJob,
    sink: &JobSink<'_>,
    dir: &Path,
    ordinal: usize,
    engine: impl FnOnce(
        &freemkv_engine::RemuxJob,
        &dyn Sink,
    ) -> std::io::Result<freemkv_engine::RemuxReport>,
) -> std::io::Result<Option<String>> {
    let id = sink
        .job_id
        .saturating_mul(1000)
        .saturating_add(ordinal as u64);
    let unsaved = || sink.landing_unsaved();
    deliver::remux_via_stage(request, sink, dir, id, &unsaved, engine).map(|r| r.writing_app)
}

fn cancelled_ending(sink: &JobSink<'_>) -> Option<Ending> {
    if sink.lib.cancelled(sink.job_id) {
        Some(Ending::Stopped(JobNote::Cancelled))
    } else if sink.preempted() {
        Some(Ending::Stopped(JobNote::Preempted))
    } else if shutting_down() {
        Some(Ending::Stopped(JobNote::Interrupted))
    } else if sink.lib.stall_cancel.load(Ordering::SeqCst) {
        Some(Ending::Stopped(JobNote::Stalled))
    } else {
        None
    }
}

fn resume_kept(kept: &Path, target: &Path, sink: &JobSink<'_>) -> std::io::Result<Option<String>> {
    if deliver::read(kept)?.target != target {
        return Err(std::io::Error::other(
            "retained output destination differs from the queued target",
        ));
    }
    deliver::resume(kept, sink, &libfreemkv::Halt::new(), &deliver::OsIo).map(|d| d.writing_app)
}

// The lifecycle is shared with tests; only media execution/delivery is substituted.
fn remux_planned_with(
    job: &Job,
    plan: &crate::server::planner::RemuxPlan,
    cfg: &Config,
    sink: &JobSink<'_>,
    mut execute: impl FnMut(
        &freemkv_engine::RemuxJob,
        Option<&Path>,
        usize,
    ) -> std::io::Result<Option<String>>,
) -> Ending {
    if plan.outputs.is_empty()
        || plan.outputs.iter().any(|o| {
            let p = Path::new(&o.filename);
            p.components().count() != 1
                || !matches!(p.components().next(), Some(std::path::Component::Normal(_)))
                || o.filename.contains('\\')
        })
    {
        return Ending::Failed(std::io::Error::other("invalid remux output plan"));
    }
    let targets = job.output_targets();
    if targets.len() != plan.outputs.len() {
        return Ending::Failed(std::io::Error::other(
            "remux output count does not match its plan",
        ));
    }
    let tv = tv_destinations(cfg, plan);
    if tv
        .as_ref()
        .is_some_and(|(_, expected)| expected != &targets)
    {
        return Ending::Failed(std::io::Error::other(
            "TV destinations changed; preserve this plan and queue a new job with the current TV folder",
        ));
    }
    let validate = |target: &Path| planned_target_ok(cfg, plan, target);
    let saved = sink.lib.queue.snapshot();
    let Some(current) = saved.jobs.iter().find(|j| j.id == job.id) else {
        return Ending::Failed(std::io::Error::other("remux job no longer exists"));
    };
    if current.outputs.len() != plan.outputs.len() {
        return Ending::Failed(std::io::Error::other(
            "remux output progress count does not match its plan",
        ));
    }
    let mut ids = std::collections::HashSet::new();
    if plan.outputs.iter().zip(&targets).any(|(o, target)| {
        !ids.insert(&o.id)
            || !current
                .outputs
                .iter()
                .any(|p| p.id == o.id && p.target == *target)
    }) {
        return Ending::Failed(std::io::Error::other(
            "remux output progress does not match its plan",
        ));
    }
    let mut seen = std::collections::HashSet::new();
    for target in &targets {
        if !seen.insert(target) {
            return Ending::Failed(std::io::Error::other("duplicate remux output target"));
        }
        if let Err(why) = validate(target) {
            return Ending::Failed(std::io::Error::other(why));
        }
    }
    // A plan frozen just now was never preflighted on its TV folder by the queue.
    if let Some(tv) = tv_root_of(&super::dirs(cfg), current)
        && let Err(p) = health::preflight(super::TV_ROLE, tv)
    {
        return Ending::Failed(problem_error(&p));
    }
    let owned = match current.owned.clone() {
        Some(owned) => owned,
        None => match capture_owned(job, cfg, &sink.lib.config_dir).and_then(|owned| {
            sink.lib
                .queue
                .set_owned(job.id, owned.clone())
                .map(|()| owned)
        }) {
            Ok(owned) => owned,
            Err(e) => return Ending::Failed(e),
        },
    };
    // Every existing target must be ours before anything is written; a staged output
    // defers to its sidecar, which recorded the target as the remux found it.
    for (output, target) in plan.outputs.iter().zip(&targets) {
        let progress = current.outputs.iter().find(|p| p.id == output.id);
        if progress.is_some_and(|p| p.state == OutputState::Done || p.staged.is_some()) {
            continue;
        }
        if landed_by_this_job(&sink.lib.config_dir, &job.iso, target)
            || progress.is_some_and(|p| landed_before(p, target))
        {
            continue;
        }
        if let Err(e) = refuse_unowned(target, &owned) {
            return Ending::Failed(e);
        }
    }
    let mut writing_app = None;
    for (ordinal, output) in plan.outputs.iter().enumerate() {
        if let Some(ending) = cancelled_ending(sink) {
            return ending;
        }
        let episode = output.episode.is_some();
        if !sink.lib.queue.begin_output(job.id, &output.id) {
            // Done before; its link may not have been written. Never vouch for the file there now.
            if !landed_by_this_job(&sink.lib.config_dir, &job.iso, &targets[ordinal])
                && let Err(e) = super::links::record_output(
                    &sink.lib.config_dir,
                    &targets[ordinal],
                    &job.iso,
                    None,
                    episode,
                )
            {
                return Ending::Failed(links_not_saved(&targets[ordinal], &e));
            }
            continue;
        }
        let target = targets[ordinal].clone();
        // A restart after the rename and before Done: the file there is the one that landed.
        if let Some(progress) = current.outputs.iter().find(|p| p.id == output.id)
            && let Some(landed) = progress
                .landing
                .filter(|_| landed_before(progress, &target))
        {
            sink.lib
                .queue
                .finish_output(job.id, &output.id, OutputState::Done);
            if let Err(e) = super::links::record_output(
                &sink.lib.config_dir,
                &target,
                &job.iso,
                Some(landed),
                episode,
            ) {
                return Ending::Failed(links_not_saved(&target, &e));
            }
            continue;
        }
        let unowned = !owned.iter().any(|o| o.path == target);
        if unowned
            && target.is_file()
            && landed_by_this_job(&sink.lib.config_dir, &job.iso, &target)
        {
            sink.lib
                .queue
                .finish_output(job.id, &output.id, OutputState::Done);
            continue;
        }
        if let Err(why) = validate(&target) {
            return Ending::Failed(std::io::Error::other(why));
        }
        let staged = job
            .outputs
            .iter()
            .find(|p| p.id == output.id)
            .and_then(|p| p.staged.as_deref());
        let replace = match staged {
            Some(_) => false,
            None => match may_replace(&target, &owned) {
                Ok(replace) => replace,
                Err(e) => return Ending::Failed(e),
            },
        };
        if let Some(parent) = target.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            return Ending::Failed(e);
        }
        let request = freemkv_engine::RemuxJob {
            iso: freemkv_engine::ImageSource::from_path(&job.iso),
            title: Some(output.title_index),
            streams: freemkv_engine::StreamChoice::default(),
            target: target.clone(),
            replace,
        };
        sink.lib.set_running(|running| {
            if let Some(running) = running.as_mut().filter(|r| r.job_id == job.id) {
                running.target = target.clone();
            }
        });
        let replaces = owned
            .iter()
            .find(|o| replace && o.path == target)
            .map(|o| o.stamp);
        sink.expect_landing(&target, &output.id, replaces);
        writing_app = match execute(&request, staged, ordinal) {
            Ok(app) => app,
            Err(_) if let Some(changed) = sink.landing_refused() => {
                sink.lib
                    .queue
                    .finish_output(job.id, &output.id, OutputState::Failed);
                return Ending::Failed(changed_during(&changed));
            }
            Err(e) if let Some(why) = sink.landing_unsaved() => {
                keep_staged(sink, job.id, &output.id, &e);
                sink.lib
                    .queue
                    .finish_output(job.id, &output.id, OutputState::Failed);
                return Ending::Failed(std::io::Error::other(crate::strings::fmt_or(
                    "server.library.landing_not_recorded",
                    "The new file for {path} was not put in place because its landing could not be recorded ({error}). Retry once the config folder takes writes again.",
                    &[("path", &target.display().to_string()), ("error", &why)],
                )));
            }
            Err(e) => {
                keep_staged(sink, job.id, &output.id, &e);
                sink.lib
                    .queue
                    .finish_output(job.id, &output.id, OutputState::Failed);
                return cancelled_ending(sink).unwrap_or(Ending::Failed(e));
            }
        };
        // The link carries the landing recorded before the rename, not a later look.
        let landed = sink.landed_stamp();
        sink.lib.note_landed(&target, writing_app.clone());
        sink.lib.queue.set_output_staged(job.id, &output.id, None);
        sink.lib.queue.set_staged(job.id, None);
        sink.lib
            .queue
            .finish_output(job.id, &output.id, OutputState::Done);
        if let Err(e) =
            super::links::record_output(&sink.lib.config_dir, &target, &job.iso, landed, episode)
        {
            return Ending::Failed(links_not_saved(&target, &e));
        }
    }
    if job.retire {
        if !outputs_all_done(&sink.lib.queue.snapshot(), job.id) {
            return Ending::Failed(std::io::Error::other(
                "not every new file is recorded as done; no old file was removed",
            ));
        }
        let roots = super::owned::roots(&super::dirs(cfg));
        let retire = super::retire::Retire {
            iso: &job.iso,
            config_dir: &sink.lib.config_dir,
            roots: &roots,
            sink,
            stopping: &|| cancelled_ending(sink).is_some(),
        };
        let retired = super::retire::run(&retire, &owned, &targets);
        // A scan that listed the removed files may commit after this; one more follows it.
        sink.lib.wake_indexer();
        match retired {
            Ok(kept) if kept.is_empty() => {}
            Ok(kept) => {
                let message = super::retire::kept_message(targets.len(), &kept);
                return Ending::Failed(std::io::Error::other(super::retire::KeptOld(message)));
            }
            Err(e) => return cancelled_ending(sink).unwrap_or(Ending::Failed(e)),
        }
    }
    Ending::Done { writing_app }
}

// Record the file a failed output's delivery kept, so a Retry only copies it in.
fn keep_staged(sink: &JobSink<'_>, job_id: u64, output_id: &str, e: &std::io::Error) {
    if let Some(kept) = deliver::kept_of(e) {
        let queue = &sink.lib.queue;
        queue.set_output_staged(job_id, output_id, Some(kept.path.clone()));
        queue.set_staged(job_id, still_kept(Some(&kept.path)));
    }
}

// A landed output whose link could not be saved: the file is there, and the config folder is not.
fn links_not_saved(target: &Path, e: &std::io::Error) -> std::io::Error {
    std::io::Error::new(
        e.kind(),
        crate::strings::fmt_or(
            "server.library.links_not_saved",
            "{path} is in place, but library-links.json in the config folder could not be updated ({error}). Fix that file or its folder's permissions, then Retry.",
            &[
                ("path", &target.display().to_string()),
                ("error", &e.to_string()),
            ],
        ),
    )
}

// True when the queue shows every output of job `id` as Done (and the job exists).
fn outputs_all_done(queue: &super::queue::QueueFile, id: u64) -> bool {
    queue
        .jobs
        .iter()
        .find(|j| j.id == id)
        .is_some_and(|j| j.outputs.iter().all(|o| o.state == OutputState::Done))
}

// True when the file at `target` is the one `progress` recorded as about to land.
fn landed_before(progress: &super::queue::OutputProgress, target: &Path) -> bool {
    progress.landing.is_some_and(|landing| {
        matches!(super::owned::present(target), Ok(super::owned::Present::File(now)) if now == landing)
    })
}

// True when links.json maps `target` to the job's ISO with the file there now: an earlier
// run of this job landed it after the owned files were captured.
fn landed_by_this_job(config_dir: &Path, iso: &Path, target: &Path) -> bool {
    let Some(link) = super::links::entries(config_dir).remove(target) else {
        return false;
    };
    link.iso == iso
        && link.stamp.is_some()
        && matches!(super::owned::present(target), Ok(super::owned::Present::File(s)) if Some(s) == link.stamp)
}

// The files a job may overwrite: links for its ISO under the output folders, plus the
// row's own MKV when queued to replace it unless a link disowns it. Unreadable links stop a
// correction or a replace; a new file's job then owns nothing.
fn capture_owned(job: &Job, cfg: &Config, config_dir: &Path) -> std::io::Result<Vec<Owned>> {
    let links = match links_for_job(config_dir) {
        Ok(links) => links,
        Err(e) if job.retire || job.replace => return Err(e),
        Err(_) => Default::default(),
    };
    let roots = super::owned::roots(&super::dirs(cfg));
    let targets = job.output_targets();
    // A retire job needs every linked file; a plain remux only its own targets.
    let mut owned = super::owned::capture_with(&links, &job.iso, &roots, |p| {
        job.retire || p == job.target || targets.iter().any(|t| t == p)
    })?;
    if job.replace
        && !owned.iter().any(|o| o.path == job.target)
        && !super::owned::disowned(&links, &job.iso, &job.target)
        && let super::owned::Present::File(stamp) = super::owned::present(&job.target)?
    {
        owned.push(Owned {
            path: job.target.clone(),
            stamp,
        });
        owned.sort_by(|a, b| a.path.cmp(&b.path));
    }
    Ok(owned)
}

fn refuse_unowned(target: &Path, owned: &[Owned]) -> std::io::Result<()> {
    match std::fs::symlink_metadata(target) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
        Ok(_) if owned.iter().any(|o| o.path == target) => Ok(()),
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!(
                "{} already exists and was not made from this ISO; nothing was written",
                target.display()
            ),
        )),
    }
}

// Overwrite only a file this job owns and that is still as it was captured.
fn may_replace(target: &Path, owned: &[Owned]) -> std::io::Result<bool> {
    refuse_unowned(target, owned)?;
    let Some(ours) = owned.iter().find(|o| o.path == target) else {
        return Ok(false);
    };
    match super::owned::present(target)? {
        super::owned::Present::File(stamp) if stamp == ours.stamp => Ok(true),
        super::owned::Present::Gone => Ok(false),
        _ => Err(changed_during(target)),
    }
}

// An owned library file that changed after it was captured: it is not overwritten.
fn changed_during(path: &Path) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        crate::strings::fmt_or(
            "server.library.changed_during",
            "{path} changed while this remux ran; it was not touched",
            &[("path", &path.display().to_string())],
        ),
    )
}

// A planned output must sit under the root its plan files into.
fn planned_target_ok(
    cfg: &Config,
    plan: &crate::server::planner::RemuxPlan,
    target: &Path,
) -> Result<(), String> {
    let d = super::dirs(cfg);
    // A TV plan files under the TV folder, or the library for a legacy single output.
    if plan.media.kind == Some(crate::server::planner::MediaKind::Tv)
        && let Some(tv) = d.tv.as_ref().filter(|tv| target.starts_with(tv))
    {
        return safe_destination(tv, target, true);
    }
    safe_to_create(&d, target)
}

// Only this directory and these numeric job files (`<id>.mkv` and its `.partial` and
// `.lock`) belong to the library worker; a killed container cannot clean them up itself.
fn clean_stale_staging(dir: &Path) {
    let Ok(files) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in files.flatten() {
        let path = entry.path();
        let owned = path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|name| {
                [".mkv", ".mkv.partial", ".mkv.lock"].iter().any(|suffix| {
                    name.strip_suffix(suffix)
                        .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
                })
            });
        if owned
            && path.is_file()
            && let Err(e) = std::fs::remove_file(&path)
        {
            tracing::warn!(path = %path.display(), error = %e, "stale remux stage could not be removed");
        }
    }
}

/// How long, and how many bytes, finished files may wait on local staging for the output folder.
pub(crate) struct StagingLimits {
    pub max_age: Duration,
    pub max_bytes: u64,
}

/// Kept files expire after this many days unless the settings say otherwise.
pub(crate) const STAGED_MAX_AGE_DAYS: u64 = 7;
/// The default cap on kept files: a quarter of the staging disk, at most this much.
pub(crate) const STAGED_MAX_BYTES: u64 = 500_000_000_000;

impl StagingLimits {
    /// The settings' limits (0 = the default) for the staging folder `dir`.
    pub(crate) fn of(cfg: &Config, dir: &Path) -> Self {
        let days = match cfg.remux_staged_max_age_days {
            0 => STAGED_MAX_AGE_DAYS,
            d => d,
        };
        let max_bytes = match cfg.remux_staged_max_gb {
            0 => health::fs_capacity(dir)
                .map_or(STAGED_MAX_BYTES, |total| (total / 4).min(STAGED_MAX_BYTES)),
            gb => gb.saturating_mul(1_000_000_000),
        };
        Self {
            max_age: Duration::from_secs(days.saturating_mul(86_400)),
            max_bytes,
        }
    }
}

/// Startup sweep of the remux staging folder, run before any remux: interrupted job files,
/// debris named like a kept file, kept files past `limits`; the rest wait in the queue to be
/// copied in. Returns how many kept files were discarded.
pub(crate) fn housekeep_staging(
    lib: &Library,
    dir: &Path,
    limits: &StagingLimits,
    library: &Path,
) -> usize {
    clean_stale_staging(dir);
    for path in deliver::orphans(dir) {
        match std::fs::remove_file(&path) {
            Ok(()) => tracing::info!(path = %path.display(), "removed an incomplete kept remux"),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "incomplete kept remux could not be removed")
            }
        }
    }
    let pending = deliver::pending(dir);
    let mut discard: Vec<&deliver::KeptInfo> = pending
        .iter()
        .filter(|i| deliver::expired(i, limits.max_age))
        .collect();
    let fresh: Vec<deliver::KeptInfo> = pending
        .iter()
        .filter(|i| !deliver::expired(i, limits.max_age))
        .cloned()
        .collect();
    discard.extend(deliver::over_budget(&fresh, limits.max_bytes));
    let mut gone = std::collections::HashSet::new();
    for info in &discard {
        match deliver::discard(&info.staged) {
            Ok(()) => {
                tracing::info!(path = %info.staged.display(), bytes = info.size, "discarded a kept remux (too old, or staging over its budget)");
                gone.insert(info.staged.clone());
            }
            Err(e) => {
                tracing::warn!(path = %info.staged.display(), error = %e, "kept remux could not be discarded")
            }
        }
    }
    let kept = pending
        .into_iter()
        .filter(|i| !gone.contains(&i.staged))
        .map(|i| adopted(i, library))
        .collect();
    let added = lib.queue.adopt_staged(kept);
    if added > 0 {
        tracing::info!(count = added, "kept remuxes queued to be copied in");
    }
    gone.len()
}

// A kept file found at startup, as the job that copies it in.
fn adopted(i: deliver::KeptInfo, library: &Path) -> (NewJob, StagedFile, Failure) {
    let job = NewJob {
        title: super::index::mkv_title(library, &i.target),
        iso: i.iso.clone(),
        target: i.target.clone(),
        replace: i.replace,
    };
    let failure = Failure {
        code: i.error_code,
        message: format!(
            "{}. Finished locally; waiting for the output folder to copy it in.",
            i.error.trim_end_matches('.')
        ),
    };
    let staged = StagedFile {
        path: i.staged,
        bytes: i.size,
        attempts: i.attempts,
    };
    (job, staged, failure)
}

#[cfg(test)]
#[path = "worker_staged_cleanup_tests.rs"]
mod staged_cleanup_tests;

/// The last look before a remux creates a new MKV: the target must sit under
/// the library folder, and that folder must be listable and not empty (an
/// unmounted share is an empty mountpoint on the local disk).
pub(crate) fn safe_to_create(d: &super::Dirs, target: &std::path::Path) -> Result<(), String> {
    safe_destination(&d.library, target, false)
}

/// [`safe_to_create`] under any configured `root` (the library or the TV folder): it must
/// be listable and not empty, so an unmounted share's mountpoint is never written into.
/// `tv` says `root` is the TV folder, for the refusal's wording.
pub(super) fn safe_destination(root: &Path, target: &Path, tv: bool) -> Result<(), String> {
    if !target.starts_with(root)
        || target
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(format!(
            "{} is not under the library folder",
            target.display()
        ));
    }
    if crate::server::health::share_unmounted(root) {
        return Err("the output share is not mounted".into());
    }
    if let Ok(root) = root.canonicalize() {
        let mut ancestor = target;
        loop {
            match ancestor.canonicalize() {
                Ok(path) => {
                    if !path.starts_with(&root) {
                        return Err("output escapes the library folder".into());
                    }
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    ancestor = ancestor
                        .parent()
                        .ok_or_else(|| "output has no existing parent".to_string())?;
                }
                Err(e) => return Err(format!("cannot resolve output destination: {e}")),
            }
        }
    }
    let path = root.display().to_string();
    match std::fs::read_dir(root).map(|mut r| r.next().is_some()) {
        Ok(true) => Ok(()),
        Ok(false) if tv => Err(crate::strings::fmt_or(
            "server.library.tv_folder_empty",
            "the TV folder {path} is empty (is the share mounted?); nothing was written",
            &[("path", &path)],
        )),
        Ok(false) => Err(crate::strings::fmt_or(
            "server.library.library_folder_empty",
            "the library folder {path} is empty (is the share mounted?); nothing was written",
            &[("path", &path)],
        )),
        Err(e) if tv => Err(crate::strings::fmt_or(
            "server.library.tv_folder_unreadable",
            "the TV folder {path} cannot be read ({error}); nothing was written",
            &[("path", &path), ("error", &e.to_string())],
        )),
        Err(e) => Err(crate::strings::fmt_or(
            "server.library.library_folder_unreadable",
            "the library folder {path} cannot be read ({error}); nothing was written",
            &[("path", &path), ("error", &e.to_string())],
        )),
    }
}

/// Write a job's ending to the probe cache, the index, the queue and the
/// console. On success the new stamp is recorded before the queue changes, so
/// the row is current the moment the UI hears the job finished.
pub(crate) fn finish(
    lib: &Library,
    job: &Job,
    ending: Ending,
    took: Duration,
    sink: &dyn LineSink,
) {
    let now = crate::server::util::epoch_secs();
    let snapshot = lib.queue.snapshot();
    let job = snapshot
        .jobs
        .iter()
        .find(|j| j.id == job.id && j.plan.is_some())
        .unwrap_or(job);
    match ending {
        Ending::Done { writing_app } => {
            let size = if job.plan.is_some() {
                job.output_targets()
                    .iter()
                    .filter_map(|p| std::fs::metadata(p).ok())
                    .fold(0u64, |n, m| n.saturating_add(m.len()))
            } else {
                lib.note_landed(&job.target, writing_app.clone());
                lib.snapshot().sigs.get(&job.target).map_or(0, |s| s.size)
            };
            lib.queue.finish(
                job.id,
                JobResult::Done {
                    size_bytes: size,
                    secs: took.as_secs(),
                    finished_at: now,
                    writing_app: writing_app.clone(),
                },
            );
            sink.line(
                LineKind::Ok,
                format!(
                    "Remuxed in {}: {}",
                    hms(took.as_secs()),
                    writing_app.as_deref().unwrap_or("no writing-app stamp")
                ),
            );
        }
        Ending::Stopped(note @ JobNote::Stalled) => {
            // A stall is retried only when a folder it uses is unhealthy now.
            let phase = phase_now(lib);
            let problem = folder_problem(lib, job);
            let kept = still_kept(job.staged.as_deref());
            if problem.is_some() {
                let why = (None, "stalled");
                if retry_storage_fault(
                    lib,
                    job,
                    why,
                    Fault::Unresponsive,
                    problem.as_ref(),
                    phase.as_deref(),
                    now,
                    kept.clone(),
                    sink,
                ) {
                    return;
                }
            }
            lib.queue.set_staged(job.id, kept);
            lib.queue.note_running(job.id, note);
            lib.queue.finish(
                job.id,
                JobResult::Failed {
                    code: None,
                    message: "stalled".into(),
                    finished_at: now,
                },
            );
            sink.line(
                LineKind::Err,
                format!("Stopped: no progress. {}", safe_text(job)),
            );
        }
        Ending::Stopped(JobNote::Cancelled) if job.staged.is_some() => {
            // A Stop leaves the kept file: it stays offered for Retry or Discard.
            let kept = still_kept(job.staged.as_deref());
            let message = if kept.is_some() {
                "Stopped. The finished MKV is still kept on local staging: Retry copies it in, \
                 Discard deletes it."
            } else {
                "Stopped."
            };
            lib.queue.set_staged(job.id, kept);
            lib.queue.finish(
                job.id,
                JobResult::Failed {
                    code: None,
                    message: message.into(),
                    finished_at: now,
                },
            );
            sink.line(LineKind::Warn, format!("{message} {}", safe_text(job)));
        }
        Ending::Stopped(JobNote::Cancelled) => {
            lib.queue.drop_job(job.id);
            // The engine/delivery owns partial cleanup and its artifact lock. A second
            // unlink here could block on NFS or race a later delivery's partial.
            sink.line(LineKind::Warn, format!("Stopped. {}", safe_text(job)));
        }
        Ending::Stopped(note) => {
            lib.queue.requeue(job.id, note);
            sink.line(
                LineKind::Warn,
                match note {
                    JobNote::Preempted => {
                        "Stopped: a rip needs the mux slot. Back at the head of the queue."
                    }
                    _ => "Interrupted. Back at the head of the queue.",
                }
                .into(),
            );
        }
        Ending::Failed(e) => {
            // A delivery that failed after the local verify keeps the file: judge its cause.
            let delivery_kept = deliver::kept_of(&e);
            let cause = delivery_kept.map_or(&e, |k| &k.cause);
            let kept = still_kept(
                delivery_kept
                    .map(|k| k.path.as_path())
                    .or(job.staged.as_deref()),
            );
            let phase = delivery_kept
                .map(|k| k.phase.to_string())
                .or_else(|| phase_of(&e))
                .or_else(|| phase_now(lib));
            let code = freemkv_engine::error_code(&e);
            let text = error_text(&e);
            let fault = storage_fault(cause);
            let unhealthy = fault.and_then(|_| folder_problem(lib, job));
            if let Some(f) = fault
                && retry_storage_fault(
                    lib,
                    job,
                    (code, &text),
                    f,
                    unhealthy.as_ref(),
                    phase.as_deref(),
                    now,
                    kept.clone(),
                    sink,
                )
            {
                return;
            }
            // Not retried; still name a folder that is unhealthy right now, and what is safe.
            let changed =
                job.staged.is_some() && code == Some(libfreemkv::error::E_REMUX_TARGET_EXISTS);
            let kept_old = e
                .get_ref()
                .is_some_and(|inner| inner.is::<super::retire::KeptOld>());
            // Nothing is left to finish after kept old files: they are no longer tracked.
            let safe = if kept_old {
                crate::strings::get_or(
                    "server.library.safe.retained",
                    "The source ISO is retained.",
                )
            } else {
                safe_text(job)
            };
            let mut message = if changed {
                target_changed_message(&text, job)
            } else if kept_old {
                format!("{text} {safe}")
            } else {
                failure_message(&text, phase.as_deref(), unhealthy.as_ref(), job, None)
            };
            if let Some(k) = &kept {
                message.push(' ');
                message.push_str(&kept_text(k, changed));
            } else if job.staged.is_some() {
                message.push_str(" The finished MKV kept on local staging could not be used and was removed; queue a fresh remux.");
            }
            lib.queue.set_staged(job.id, kept.clone());
            lib.queue.finish(
                job.id,
                JobResult::Failed {
                    code,
                    message: message.clone(),
                    finished_at: now,
                },
            );
            if !sink.error_shown() {
                sink.line(LineKind::Err, format!("{}: {text}", error_word()));
            }
            if let Some(p) = &unhealthy {
                sink.line(LineKind::Err, format!("{} {}", p.message, p.hint));
            }
            if changed {
                sink.line(LineKind::Err, target_changed_message(&text, job));
            }
            sink.line(LineKind::Out, safe);
            if let Some(k) = &kept {
                sink.line(LineKind::Warn, kept_text(k, changed));
            }
        }
    }
}

/// The kept file at `path` (either half of the pair), if it is still a usable one.
fn still_kept(path: Option<&Path>) -> Option<StagedFile> {
    let path = path.filter(|p| deliver::is_kept(p))?;
    let info = deliver::read(path).ok()?;
    Some(StagedFile {
        path: info.staged,
        bytes: info.size,
        attempts: info.attempts,
    })
}

fn gb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1e9)
}

/// What a kept file means for the user: where it is, and what Retry and Discard do.
pub(crate) fn kept_text(k: &StagedFile, changed: bool) -> String {
    if changed {
        format!(
            "The finished MKV ({}) is still kept on local staging; Discard it once you have checked the library file.",
            gb(k.bytes)
        )
    } else {
        format!(
            "The finished MKV ({}) is kept on local staging: Retry copies it in without muxing again, Discard deletes it.",
            gb(k.bytes)
        )
    }
}

// E9084 on a kept file: the library file is not the one the remux set out to replace.
fn target_changed_message(text: &str, job: &Job) -> String {
    // The output whose kept file was being copied in, else the one that failed.
    let changed = job
        .outputs
        .iter()
        .find(|o| o.staged.is_some() && o.staged == job.staged)
        .or_else(|| job.outputs.iter().find(|o| o.state == OutputState::Failed))
        .map_or(&job.target, |o| &o.target);
    crate::strings::fmt_or(
        "server.library.target_changed",
        "{error}. The library file {path} changed after this remux began, or the finished MKV was already copied in, so it was not touched.",
        &[
            ("error", text.trim_end_matches('.')),
            ("path", &changed.display().to_string()),
        ],
    )
}

/// The engine phase the job was in, as the running state last showed it.
fn phase_now(lib: &Library) -> Option<String> {
    lib.running()
        .map(|r| r.phase)
        .filter(|p| !p.is_empty() && p != "start")
}

// A timeout names the operation it stalled in (`E9073: copy`).
fn phase_of(e: &std::io::Error) -> Option<String> {
    let text = e.to_string();
    let (code, data) = freemkv_engine::parse_error_code(&text)?;
    (code == libfreemkv::error::E_TIMED_OUT && phase_words(data.trim()).is_some())
        .then(|| data.trim().to_string())
}

fn phase_words(phase: &str) -> Option<&'static str> {
    Some(match phase {
        "open" => "opening the ISO",
        "mux" => "muxing",
        "sync" => "flushing the new file to disk",
        "verify" => "verifying the new file",
        "copy" => "copying the new MKV into the output folder",
        "replace" => "moving the new MKV into place",
        "target" => "checking the library file it replaces",
        _ => return None,
    })
}

/// Whether a failure is the storage's rather than the content's. A coded engine error is
/// about the disc image or the mux, except the timeouts of a stalled read, write or flush.
pub(crate) fn storage_fault(e: &std::io::Error) -> Option<Fault> {
    use libfreemkv::error::{E_SYNC_TIMEOUT, E_TIMED_OUT};
    match freemkv_engine::error_code(e) {
        Some(E_TIMED_OUT | E_SYNC_TIMEOUT) => Some(Fault::Unresponsive),
        Some(_) => None,
        None => Some(Fault::of(e)).filter(|f| *f != Fault::Other),
    }
}

/// A fresh, bounded look at every folder `job` uses: the first that fails, if any.
pub(crate) fn folder_problem(lib: &Library, job: &Job) -> Option<Problem> {
    let output = lib
        .snapshot()
        .dirs
        .as_ref()
        .map(|d| d.library.clone())
        .or_else(|| job.target.parent().map(Path::to_path_buf))?;
    if let Err(p) = health::preflight("output", &output) {
        return Some(p);
    }
    let dirs = lib.snapshot().dirs.clone();
    if let Some(tv) = dirs.as_ref().and_then(|d| tv_root_of(d, job))
        && let Err(p) = health::preflight(super::TV_ROLE, tv)
    {
        return Some(p);
    }
    if let Some(dir) = job.iso.parent() {
        let iso_dir = dir.to_path_buf();
        let probe = move |d: &Path| std::fs::read_dir(d).map(drop);
        if let Err(p) =
            health::preflight_with("source ISO", &iso_dir, health::PREFLIGHT_TIMEOUT, probe)
        {
            return Some(p);
        }
    }
    let stage = health::remux_stage_dir()?;
    health::preflight("remux staging", &stage).err()
}

// Requeue a job a storage fault stopped, given `problem` from re-checking its folders once:
// at once if they are fine now (first time only), else after the backoff, once they check
// out. False when the folders are fine and it failed before, or the retries are used up.
#[allow(clippy::too_many_arguments)]
fn retry_storage_fault(
    lib: &Library,
    job: &Job,
    (code, text): (Option<u16>, &str),
    fault: Fault,
    problem: Option<&Problem>,
    phase: Option<&str>,
    now: u64,
    staged: Option<StagedFile>,
    sink: &dyn LineSink,
) -> bool {
    // With every folder fine, a missing file or a refused write is the file's problem.
    let file_only = matches!(fault, Fault::Missing | Fault::Denied | Fault::ReadOnly);
    if problem.is_none() && (job.attempts > 0 || file_only) {
        return false;
    }
    let Some(at) = super::queue::retry_at(job, now, problem.is_none()) else {
        sink.line(
            LineKind::Err,
            format!(
                "Giving up after {} automatic retries over {}.",
                job.attempts,
                hms(now.saturating_sub(job.first_failed_at.unwrap_or(now)))
            ),
        );
        return false;
    };
    let mut message = failure_message(text, phase, problem, job, Some(at));
    if let Some(k) = &staged {
        message.push_str(&format!(
            " The finished MKV ({}) waits on local staging to be copied in.",
            gb(k.bytes)
        ));
    }
    let kept = staged.as_ref().map(|k| k.bytes);
    lib.queue
        .retry_later(job.id, Failure { code, message }, at, staged);
    if let Some(p) = problem {
        lib.hold_for(p, now);
        sink.line(LineKind::Err, format!("{} {}", p.message, p.hint));
    }
    sink.line(LineKind::Out, safe_text(job));
    if let Some(bytes) = kept {
        sink.line(
            LineKind::Out,
            format!(
                "Finished locally ({}): only the copy into the output folder is retried.",
                gb(bytes)
            ),
        );
    }
    sink.line(
        LineKind::Warn,
        if at <= now {
            "The folders answer again: retrying now.".to_string()
        } else {
            format!(
                "Waiting for the folder; retry {} in {} once it checks out.",
                job.attempts + 1,
                hms(at - now)
            )
        },
    );
    true
}

// What is safe after `job` stopped, from the outputs its queue entry shows landed.
fn safe_text(job: &Job) -> String {
    use crate::strings::get_or;
    let done = job
        .outputs
        .iter()
        .filter(|o| o.state == OutputState::Done)
        .count();
    let all_done = done > 0 && done == job.outputs.len();
    if job.retire && all_done {
        get_or(
            "server.library.safe.retire_done",
            "Every new file is in place; run Change match again to remove the old files made from this ISO that remain. The source ISO is retained.",
        )
    } else if job.retire {
        get_or(
            "server.library.safe.retire_pending",
            "The source ISO is retained. Old files made from this ISO are removed only after every new file is in place, and only if unchanged; run Change match again to finish.",
        )
    } else if job.plan.as_ref().is_some_and(|plan| plan.outputs.len() > 1) {
        get_or(
            "server.library.safe.outputs",
            "The source ISO is retained. Completed outputs remain in the library; unfinished outputs can be retried.",
        )
    } else if done > 0 {
        get_or(
            "server.library.safe.landed",
            "The new MKV is in the library.",
        )
    } else if job.replace {
        get_or(
            "server.library.safe.unchanged",
            "Your existing MKV is unchanged.",
        )
    } else {
        get_or(
            "server.library.safe.nothing_written",
            "No MKV was written to the library.",
        )
    }
}

/// A failed job's message: the error, the phase it stopped in, the folder at fault, what
/// is safe, and when it is retried.
pub(crate) fn failure_message(
    text: &str,
    phase: Option<&str>,
    folder: Option<&Problem>,
    job: &Job,
    retry_at: Option<u64>,
) -> String {
    let mut out = text.trim_end_matches('.').to_string();
    out.push('.');
    if let Some(words) = phase.and_then(phase_words) {
        out.push_str(&format!(" It stopped while {words}."));
    }
    // A job that failed on this folder's check already names it.
    if let Some(sentence) = folder
        .map(folder_sentence)
        .filter(|s| !out.contains(s.as_str()))
    {
        out.push(' ');
        out.push_str(&sentence);
    }
    out.push(' ');
    out.push_str(&safe_text(job));
    if retry_at.is_some() {
        out.push_str(" It is retried automatically once the folder checks out.");
    }
    out
}

// `E<code> <message>` from the locale when it has the code, else the raw text.
fn error_text(e: &std::io::Error) -> String {
    let raw = e.to_string();
    let Some((code, data)) = freemkv_engine::parse_error_code(&raw) else {
        return raw;
    };
    let key = format!("error.E{code}");
    let text = crate::strings::get(&key);
    if text == key {
        return raw;
    }
    // E6000's status/sense hex tail is diagnostic noise; the CLI shows only the sector.
    let detail = if code == 6000 {
        data.split_whitespace().next().unwrap_or(data)
    } else {
        data
    };
    let text = crate::strings::fmt(&key, &[("detail", detail), ("hash", data)]);
    let level = format!("{}: ", error_word());
    format!("E{code} {}", text.strip_prefix(&level).unwrap_or(&text))
}

fn error_word() -> String {
    crate::strings::get(crate::messaging::Level::Error.locale_key())
}

// The CLI's `render_error`: the level word once, then the coded message.
fn error_line(e: &std::io::Error) -> String {
    format!("{}: {}", error_word(), error_text(e))
}

fn hms(secs: u64) -> String {
    format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

/// Somewhere a job's human-readable lines go.
pub(crate) trait LineSink {
    fn line(&self, kind: LineKind, text: String);
    /// The job's error has already been printed (the engine reported it as
    /// the title failed), so the ending must not print it again.
    fn error_shown(&self) -> bool {
        false
    }
}

struct JobLog(Mutex<Option<std::fs::File>>);

impl JobLog {
    fn create(path: &std::path::Path) -> Self {
        // Append: a retry keeps the earlier attempts' transcripts above it.
        let earlier = std::fs::metadata(path).is_ok_and(|m| m.len() > 0);
        let file = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
            })
            .map(|mut f| {
                if earlier {
                    let _ = writeln!(f, "{}", super::log_line(LineKind::Out, ""));
                    let _ = writeln!(
                        f,
                        "{}",
                        super::log_line(
                            LineKind::Debug,
                            &format!("──── another attempt, {} ────", crate::server::util::format_iso_datetime())
                        )
                    );
                }
                f
            })
            .inspect_err(|e| {
                tracing::warn!(path = %path.display(), error = %e, "library job log not writable")
            })
            .ok();
        Self(Mutex::new(file))
    }

    fn write(&self, kind: LineKind, text: &str) {
        if let Some(f) = self.0.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            let _ = writeln!(f, "{}", super::log_line(kind, text));
        }
    }
}

// What the transcript has printed so far, to lay lines out the way the CLI does.
#[derive(Default)]
struct Term {
    // The title's header lines, known once the engine hands over the title.
    title: Option<TitleText>,
    opened: bool,
    mux_started: Option<Instant>,
    // The in-place progress line; printed for good when the title finishes.
    progress: String,
    error_shown: bool,
}

struct TitleText {
    duration: String,
    size_gb: String,
    streams: Vec<String>,
}

struct JobSink<'a> {
    lib: &'a Library,
    job_id: u64,
    arbiter: &'a Arbiter,
    epoch: u64,
    log: JobLog,
    debug: bool,
    iso_url: String,
    last_activity: AtomicU64,
    term: Mutex<Term>,
    landing: Mutex<Option<Landing>>,
}

// The output a remux is about to land, checked once more at the engine's "replace" phase.
struct Landing {
    target: std::path::PathBuf,
    output_id: String,
    /// The owned file it replaces, as captured; `None` when it creates the file.
    replaces: Option<crate::server::util::Stamp>,
    refused: bool,
    /// Why the landing could not be recorded durably; it must not land then.
    unsaved: Option<String>,
    /// The new file's stamp as recorded durably just before the rename.
    recorded: Option<crate::server::util::Stamp>,
}

impl JobSink<'_> {
    // Expect `target` (output `output_id`) to land next, replacing `replaces` if set.
    fn expect_landing(
        &self,
        target: &Path,
        output_id: &str,
        replaces: Option<crate::server::util::Stamp>,
    ) {
        *self.landing.lock().unwrap_or_else(|e| e.into_inner()) = Some(Landing {
            target: target.to_path_buf(),
            output_id: output_id.to_string(),
            replaces,
            refused: false,
            unsaved: None,
            recorded: None,
        });
    }

    // The stamp the landing about to happen, or just done, was recorded with.
    fn landed_stamp(&self) -> Option<crate::server::util::Stamp> {
        let landing = self.landing.lock().unwrap_or_else(|e| e.into_inner());
        landing.as_ref().and_then(|l| l.recorded)
    }

    // Why the landing about to happen could not be recorded, if it could not.
    fn landing_unsaved(&self) -> Option<String> {
        let landing = self.landing.lock().unwrap_or_else(|e| e.into_inner());
        landing.as_ref().and_then(|l| l.unsaved.clone())
    }

    // Whether the last check before landing found the replaced file changed.
    fn landing_refused(&self) -> Option<std::path::PathBuf> {
        let landing = self.landing.lock().unwrap_or_else(|e| e.into_inner());
        landing
            .as_ref()
            .filter(|l| l.refused)
            .map(|l| l.target.clone())
    }

    // Just before the rename: refuse a replaced file that changed since it was captured, else
    // record the new file's stamp so a restart after the rename knows it landed.
    fn before_landing(&self) {
        let mut guard = self.landing.lock().unwrap_or_else(|e| e.into_inner());
        let Some(landing) = guard.as_mut() else {
            return;
        };
        if let Some(captured) = landing.replaces {
            landing.refused = match super::owned::present(&landing.target) {
                Ok(super::owned::Present::File(now)) => now != captured,
                Ok(super::owned::Present::Gone) => false,
                _ => true,
            };
        }
        if landing.refused {
            return;
        }
        let partial = super::queue::partial_path(&landing.target);
        let recorded = crate::server::util::Stamp::read(&partial).and_then(|stamp| {
            self.lib
                .queue
                .set_output_landing(self.job_id, &landing.output_id, stamp)
                .map(|()| stamp)
        });
        match recorded {
            Ok(stamp) => landing.recorded = Some(stamp),
            Err(e) => landing.unsaved = Some(e.to_string()),
        }
    }
    fn preempted(&self) -> bool {
        self.arbiter.rip_active() || self.arbiter.rip_started_since(self.epoch)
    }

    fn alive(&self) {
        self.last_activity
            .store(crate::server::util::epoch_secs(), Ordering::Relaxed);
    }

    fn phase(&self, name: &str) {
        self.lib.set_running(|r| {
            if let Some(r) = r {
                r.phase = name.to_string();
            }
        });
    }

    fn term(&self) -> std::sync::MutexGuard<'_, Term> {
        self.term.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn emit(&self, kind: LineKind, text: String) {
        self.log.write(kind, &text);
        self.lib.console(self.job_id, kind, text);
    }

    // The image is open: the CLI's "Title N (...)", "Opening iso://...OK" and stream block.
    fn print_opened(&self, idx: usize) {
        let title = {
            let mut t = self.term();
            if t.opened {
                return;
            }
            t.opened = true;
            t.title.take()
        };
        if let Some(t) = &title {
            self.emit(
                LineKind::Out,
                crate::strings::fmt(
                    "rip.title_info",
                    &[
                        ("num", &(idx + 1).to_string()),
                        ("duration", &t.duration),
                        ("size", &t.size_gb),
                    ],
                ),
            );
        }
        self.emit(LineKind::Out, format!("{}{}", opening(&self.iso_url), ok()));
        for line in title.iter().flat_map(|t| t.streams.iter()) {
            self.emit(LineKind::Out, line.clone());
        }
    }

    // End the job's transcript the way a terminal leaves it: an unfinished
    // "Opening ..." line, and the last progress line, stay on screen.
    fn close_open_line(&self) {
        let (opened, progress) = {
            let mut t = self.term();
            (t.opened, std::mem::take(&mut t.progress))
        };
        if !opened {
            self.emit(LineKind::Out, opening(&self.iso_url));
        }
        if !progress.is_empty() {
            self.emit(LineKind::Out, progress);
        }
    }
}

fn opening(url: &str) -> String {
    crate::strings::fmt("rip.opening", &[("device", url)])
}

fn ok() -> String {
    crate::strings::get("rip.ok")
}

impl LineSink for JobSink<'_> {
    fn line(&self, kind: LineKind, text: String) {
        self.emit(kind, text);
    }

    fn error_shown(&self) -> bool {
        self.term().error_shown
    }
}

impl Sink for JobSink<'_> {
    fn log(&self, level: Level, msg: &str) {
        self.alive();
        match level {
            Level::Trace | Level::Debug if !self.debug => {}
            Level::Trace | Level::Debug => self.emit(LineKind::Debug, msg.to_string()),
            Level::Warn => self.emit(LineKind::Warn, msg.to_string()),
            Level::Error => self.emit(LineKind::Err, msg.to_string()),
            // The engine's own progress notes are not what the CLI prints.
            Level::Info if self.debug => self.emit(LineKind::Debug, msg.to_string()),
            Level::Info => {}
        }
    }

    fn title_opened(&self, title: &libfreemkv::DiscTitle) {
        self.alive();
        self.term().title = Some(TitleText {
            duration: title.duration_display(),
            size_gb: format!("{:.1}", title.size_gb()),
            streams: transcript::stream_lines(title),
        });
    }

    fn progress(&self, p: &Progress) {
        self.alive();
        let line = transcript::progress_line(p.bytes_done, p.bytes_total, p.speed_bps, p.eta_secs);
        let pct = (p.bytes_total > 0)
            .then(|| (p.bytes_done as f64 * 100.0 / p.bytes_total as f64).min(100.0));
        self.term().progress = line.clone();
        if self.debug {
            self.log.write(LineKind::Debug, &line);
        }
        self.lib.set_running(|r| {
            if let Some(r) = r {
                r.pct = pct;
                r.bytes_done = p.bytes_done;
                r.bytes_total = p.bytes_total;
                r.speed_bps = p.speed_bps;
                r.eta_secs = p.eta_secs;
                r.stalled_secs = 0;
                r.line = line;
            }
        });
    }

    fn event(&self, e: &Event<'_>) {
        self.alive();
        match e {
            Event::Phase { name } => {
                if *name == "replace" {
                    self.before_landing();
                }
                self.phase(name)
            }
            Event::TitleStart { idx, dest } => {
                self.print_opened(*idx);
                self.term().mux_started = Some(Instant::now());
                self.emit(LineKind::Out, format!("{}{}", opening(dest), ok()));
            }
            Event::TitleDone { result, .. } => {
                let (progress, started) = {
                    let mut t = self.term();
                    (std::mem::take(&mut t.progress), t.mux_started)
                };
                if !progress.is_empty() {
                    self.emit(LineKind::Out, progress);
                }
                self.lib.set_running(|r| {
                    if let Some(r) = r {
                        r.line.clear();
                    }
                });
                match result {
                    Ok(o) => {
                        self.emit(LineKind::Out, String::new());
                        let secs = started.map_or(0.0, |s| s.elapsed().as_secs_f64());
                        let kind = if o.completed {
                            LineKind::Ok
                        } else {
                            LineKind::Warn
                        };
                        self.emit(kind, transcript::complete_line(o.bytes_written, secs));
                    }
                    Err(err) => {
                        self.term().error_shown = true;
                        self.emit(LineKind::Err, error_line(err));
                    }
                }
            }
            Event::Verify {
                ok,
                runtime_secs,
                expected_secs,
                path,
            } => self.emit(
                if *ok { LineKind::Out } else { LineKind::Err },
                transcript::verify_line(path, *ok, *runtime_secs, *expected_secs),
            ),
            Event::Replaced { path } => {
                self.emit(LineKind::Out, format!("Replaced {}", path.display()))
            }
            _ => {}
        }
    }

    fn should_cancel(&self) -> bool {
        self.preempted()
            || shutting_down()
            || self.lib.stall_cancel.load(Ordering::SeqCst)
            || self.lib.cancelled(self.job_id)
            || self.landing_refused().is_some()
            || self.landing_unsaved().is_some()
    }
}

/// How long a remux may go without any engine callback before it is warned
/// about, and before it is cancelled.
pub(crate) const STALL_WARN_SECS: u64 = 300;
pub(crate) const STALL_CANCEL_SECS: u64 = crate::server::ripper::mux::HARD_WATCHDOG_STALL_SECS;

/// What the watchdog does about a remux that has been quiet for `stall_secs`.
/// There is no process exit here: a stuck remux must never take a live rip down.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StallAction {
    None,
    Warn,
    CancelRemux,
}

pub(crate) fn stall_action(stall_secs: u64, warned: bool, cancelled: bool) -> StallAction {
    if stall_secs >= STALL_CANCEL_SECS && !cancelled {
        StallAction::CancelRemux
    } else if stall_secs >= STALL_WARN_SECS && !warned {
        StallAction::Warn
    } else {
        StallAction::None
    }
}

// Runs beside the remux until `done`; its only lever is the job's own cancel flag.
fn watchdog(lib: &Library, job_id: u64, sink: &JobSink<'_>, done: &AtomicBool) {
    let (mut warned, mut cancelled) = (false, false);
    while !done.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(500));
        let now = crate::server::util::epoch_secs();
        let stall = now.saturating_sub(sink.last_activity.load(Ordering::Relaxed));
        if stall >= 60 {
            lib.set_running(|r| {
                if let Some(r) = r {
                    r.stalled_secs = stall;
                }
            });
        }
        match stall_action(stall, warned, cancelled) {
            StallAction::None => warned &= stall >= STALL_WARN_SECS,
            StallAction::Warn => {
                warned = true;
                sink.line(LineKind::Warn, format!("No progress for {}", hms(stall)));
            }
            StallAction::CancelRemux => {
                cancelled = true;
                lib.stall_cancel.store(true, Ordering::SeqCst);
                lib.queue.note_running(job_id, JobNote::Stalled);
                sink.line(
                    LineKind::Err,
                    format!("No progress for {}; cancelling this remux", hms(stall)),
                );
            }
        }
    }
}

/// How often the indexer rescans when nothing wakes it.
pub(crate) const RESCAN_SECS: u64 = 60;

/// Keep the in-memory index current: scan, read headers, audit, then sleep
/// until woken or the rescan interval passes.
pub fn index_loop(lib: &Arc<Library>, cfg: &Arc<RwLock<Config>>) {
    let d = super::dirs(&cfg.read().unwrap_or_else(|e| e.into_inner()));
    let swept = lib.sweep_partials(&d);
    if swept > 0 {
        tracing::info!(count = swept, "removed orphaned remux partials at startup");
    }
    while !shutting_down() {
        let d = super::dirs(&cfg.read().unwrap_or_else(|e| e.into_inner()));
        if !lib.index_now(&d) {
            continue;
        }
        let until = Instant::now() + Duration::from_secs(RESCAN_SECS);
        while !shutting_down() && Instant::now() < until {
            if lib.wait_for_wake(Duration::from_secs(1)) {
                break;
            }
        }
    }
}

/// The quick lane: one quick audit at a time, after rips and remuxes (Rip > Remux > Audit),
/// whatever the deep audit setting. It also tracks that setting and refills both lanes.
pub fn audit_loop(lib: &Arc<Library>, cfg: &Arc<RwLock<Config>>, arbiter: &Arbiter) {
    let enabled = || cfg.read().unwrap_or_else(|e| e.into_inner()).deep_audit;
    let mut filled = Instant::now();
    while !shutting_down() {
        let was = lib.deep_enabled();
        lib.set_deep_enabled(enabled());
        // Turning deep audit on owes every file its decode; refill now, else each minute.
        if !lib.audits.paused()
            && ((lib.deep_enabled() && !was) || filled.elapsed() >= Duration::from_secs(60))
        {
            filled = Instant::now();
            lib.refill_audits();
        }
        let library = super::dirs(&cfg.read().unwrap_or_else(|e| e.into_inner())).library;
        if !quick_turn(lib, &library, remux_or_rip_busy(lib, arbiter)) {
            crate::server::sleep_unless_shutdown(Duration::from_secs(2));
        }
    }
}

/// The deep lane: one full decode at a time while deep audit is on, after rips and remuxes.
/// A rip, a remux, a pause or a stop interrupts it; the quick lane never does.
pub(crate) fn deep_audit_loop(lib: &Arc<Library>, cfg: &Arc<RwLock<Config>>, arbiter: &Arbiter) {
    let enabled = || cfg.read().unwrap_or_else(|e| e.into_inner()).deep_audit;
    let busy = || remux_or_rip_busy(lib, arbiter);
    while !shutting_down() {
        let ffmpeg = super::deep::ffmpeg();
        if !enabled() || busy() || ffmpeg.is_none() {
            crate::server::sleep_unless_shutdown(Duration::from_secs(2));
            continue;
        }
        let Some(path) = lib.audits.next_deep() else {
            crate::server::sleep_unless_shutdown(Duration::from_secs(2));
            continue;
        };
        let library = super::dirs(&cfg.read().unwrap_or_else(|e| e.into_inner())).library;
        let stop = || {
            shutting_down()
                || !enabled()
                || busy()
                || lib.audits.paused()
                || lib.audits.cancelled_deep()
        };
        deep_one(lib, &path, &library, &stop, ffmpeg);
        lib.touch_index();
    }
}

// Run the next quick audit unless a rip or remux has the disks; true if one ran.
fn quick_turn(lib: &Library, library: &Path, busy: bool) -> bool {
    if busy {
        return false;
    }
    let Some(path) = lib.audits.next() else {
        return false;
    };
    quick_one(lib, &path, library, lib.deep_enabled());
    lib.touch_index();
    true
}

// A remux running, or queued and not paused, goes before an audit; so does any rip.
fn remux_or_rip_busy(lib: &Library, arbiter: &Arbiter) -> bool {
    arbiter.rip_active() || lib.queue.has_work()
}

fn title_of(lib: &Library, path: &Path, library: &Path) -> String {
    lib.snapshot()
        .rows
        .iter()
        .find(|r| r.mkv.as_deref() == Some(path))
        .map(|r| r.title.clone())
        .unwrap_or_else(|| super::index::mkv_title(library, path))
}

// The quick read of one file; with deep audit on, a file it leaves owing a decode joins
// the deep lane.
pub(crate) fn quick_one(lib: &Library, path: &Path, library: &Path, deep_on: bool) {
    let Some(sig) = super::probe::FileSig::stat(path) else {
        lib.audits.finish();
        return;
    };
    lib.audits.start(path, title_of(lib, path, library));
    lib.touch_index();
    // A "Stop all" that came after the file was taken but before it started.
    if lib.audits.cancelled() {
        lib.audits.finish();
        return;
    }
    let now = crate::server::util::epoch_secs;
    // On a storage failure (no report) the next refill queues the file again, a minute on.
    if let Some(report) = super::probe::audit_fast(path) {
        lib.audits.record_fast(path, sig, report, now());
        if deep_on && lib.audits.deep_due_for(path, sig, now()) {
            lib.audits.enqueue_deep(path);
        }
    }
    lib.audits.finish();
}

// The full decode of one file whose quick audit stands. An interrupted decode goes back
// to the front of the deep lane; a stopped one does not.
fn deep_one(
    lib: &Library,
    path: &Path,
    library: &Path,
    stop: &dyn Fn() -> bool,
    ffmpeg: Option<std::path::PathBuf>,
) {
    let now = crate::server::util::epoch_secs;
    let sig = super::probe::FileSig::stat(path);
    let (Some(ffmpeg), Some(sig)) = (ffmpeg, sig) else {
        lib.audits.finish_deep();
        return;
    };
    // A file changed or re-audited since it was queued waits for its quick read.
    if !lib.audits.deep_due_for(path, sig, now()) {
        lib.audits.finish_deep();
        return;
    }
    lib.audits.start_deep(path, title_of(lib, path, library));
    lib.touch_index();
    if lib.audits.cancelled_deep() {
        lib.audits.finish_deep();
        return;
    }
    let duration = lib.audits.report(path, sig).and_then(|r| r.duration_secs);
    tracing::info!(file = %path.display(), "deep audit: decoding");
    let _ = std::fs::create_dir_all(&lib.log_dir);
    let err_file = lib.log_dir.join("deep-audit.stderr");
    let progress = |stage: &'static str, secs: f64| lib.audits.progress(stage, secs, duration);
    match super::deep::full_decode(&ffmpeg, path, library, duration, &err_file, stop, &progress) {
        Some(v) => lib.audits.record_deep(path, sig, v, now()),
        None if !lib.audits.cancelled_deep() => lib.audits.requeue_front(path.to_path_buf()),
        None => {}
    }
    lib.audits.finish_deep();
}

#[cfg(test)]
#[path = "worker_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "worker_upgrade_tests.rs"]
mod upgrade_tests;
