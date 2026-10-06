use super::{rediscover_drive_with, sleep_unless_halted};
use libfreemkv::DiscPresence::{self, Absent, Present, Settling};
use std::collections::HashMap;

// Rediscovery from /dev/sg4 against scripted per-path presence answers (the last
// answer repeats; unscripted paths are Absent). Returns the result and probe counts.
fn rediscover(
    script: &[(&str, &[DiscPresence])],
    vids: &[(&str, &str)],
    keep_waiting: bool,
) -> (Option<String>, HashMap<String, usize>) {
    rediscover_from("/dev/sg4", Some("VID"), script, vids, keep_waiting)
}

// `rediscover` from `path`, expecting the disc `expected` (None: no identity cached).
fn rediscover_from(
    path: &str,
    expected: Option<&str>,
    script: &[(&str, &[DiscPresence])],
    vids: &[(&str, &str)],
    keep_waiting: bool,
) -> (Option<String>, HashMap<String, usize>) {
    let mut probes: HashMap<String, usize> = HashMap::new();
    let found = rediscover_drive_with(
        "sg4",
        path,
        expected,
        |p| {
            let n = probes.entry(p.to_string()).or_default();
            *n += 1;
            let seq = script.iter().find(|(q, _)| *q == p).map(|(_, s)| *s);
            Ok(seq.map_or(Absent, |s| s[(*n - 1).min(s.len() - 1)]))
        },
        |p| {
            vids.iter()
                .find(|(q, _)| *q == p)
                .map(|(_, v)| v.to_string())
        },
        || keep_waiting,
    );
    (found, probes)
}

// The original node spinning up after a USB reset is our drive; the re-open
// path (open_drive_with_backoff, wait_ready) already waits for it.
#[test]
fn the_original_node_spinning_up_is_accepted_without_waiting() {
    let (found, probes) = rediscover(&[("/dev/sg4", &[Settling, Present])], &[], true);
    assert_eq!(found.as_deref(), Some("/dev/sg4"));
    assert_eq!(probes["/dev/sg4"], 1);
}

#[test]
fn a_neighbour_spinning_up_is_accepted_once_present() {
    let (found, probes) = rediscover(
        &[("/dev/sg3", &[Settling, Settling, Present])],
        &[("/dev/sg3", "VID")],
        true,
    );
    assert_eq!(found.as_deref(), Some("/dev/sg3"));
    assert_eq!(
        probes["/dev/sg3"], 3,
        "a settling neighbour must be re-probed"
    );
}

#[test]
fn a_settling_neighbour_that_turns_out_empty_is_not_accepted() {
    let (found, _) = rediscover(
        &[("/dev/sg3", &[Settling, Absent]), ("/dev/sg5", &[Present])],
        &[("/dev/sg3", "VID"), ("/dev/sg5", "VID")],
        true,
    );
    assert_eq!(found.as_deref(), Some("/dev/sg5"));
}

#[test]
fn a_neighbour_that_never_settles_is_retried_a_bounded_number_of_times() {
    let (found, probes) = rediscover(&[("/dev/sg3", &[Settling])], &[], true);
    assert_eq!(
        found, None,
        "a never-settling neighbour must not be accepted"
    );
    assert_eq!(
        probes["/dev/sg3"],
        super::SETTLE_RETRIES as usize + 1,
        "the first probe plus SETTLE_RETRIES re-probes"
    );
}

// A shifted neighbour is accepted only when it carries the expected disc: a different
// or unreadable Volume ID is passed over.
#[test]
fn a_neighbour_with_another_or_unreadable_disc_is_rejected() {
    let (found, _) = rediscover(&[("/dev/sg3", &[Present])], &[("/dev/sg3", "OTHER")], true);
    assert_eq!(found, None, "an unrelated disc must not be latched");
    let (found, _) = rediscover(&[("/dev/sg3", &[Present])], &[], true);
    assert_eq!(found, None, "an unconfirmed identity must not be latched");
}

// With no identity cached a present neighbour is the legacy unverified fallback.
#[test]
fn with_no_identity_cached_a_present_neighbour_is_accepted() {
    let (found, _) = rediscover_from("/dev/sg4", None, &[("/dev/sg3", &[Present])], &[], true);
    assert_eq!(found.as_deref(), Some("/dev/sg3"));
}

// Only /dev/sgN is rediscovered, and never below sg0.
#[test]
fn rediscovery_needs_an_sg_path_and_never_probes_below_sg0() {
    let (found, probes) = rediscover_from("/dev/sr0", Some("VID"), &[], &[], true);
    assert_eq!(found, None);
    assert!(probes.is_empty(), "a non-sgN path probes nothing");
    let (_, probes) = rediscover_from("/dev/sg1", Some("VID"), &[], &[], true);
    let mut probed: Vec<&str> = probes.keys().map(String::as_str).collect();
    probed.sort_unstable();
    assert_eq!(
        probed,
        ["/dev/sg0", "/dev/sg1", "/dev/sg2", "/dev/sg3", "/dev/sg4"]
    );
}

#[test]
fn a_halt_stops_rediscovery_while_a_neighbour_settles() {
    let (found, probes) = rediscover(
        &[("/dev/sg3", &[Settling]), ("/dev/sg5", &[Present])],
        &[("/dev/sg5", "VID")],
        false,
    );
    assert_eq!(
        found, None,
        "a halted rediscovery must not hand back a drive"
    );
    assert_eq!(probes["/dev/sg3"], 1, "no re-probe after the halt");
    assert!(
        !probes.contains_key("/dev/sg5"),
        "no further candidates after the halt"
    );
}

#[test]
fn sleep_unless_halted_wakes_promptly_on_a_halt() {
    let halt = libfreemkv::Halt::new();
    halt.cancel();
    let t0 = std::time::Instant::now();
    assert!(!sleep_unless_halted(
        &halt,
        std::time::Duration::from_secs(5)
    ));
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(1),
        "{:?}",
        t0.elapsed()
    );
}
