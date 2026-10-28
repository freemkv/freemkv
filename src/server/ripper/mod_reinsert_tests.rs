// #75: ejecting and re-inserting a disc after a fault (or after Stop) starts on_insert
// again, as 1.7.7 did, without ever starting a second rip on a busy drive.
use super::*;
use libfreemkv::DiscPresence::{Present, Settling};
use libfreemkv::{DiscPresence, ScsiSense};

// One drive as the poll loop sees it: its latch, and how often on_insert started.
struct Poller {
    dev: String,
    cfg: Arc<RwLock<Config>>,
    had_disc: bool,
    started: usize,
}

impl Poller {
    fn new(name: &str) -> Self {
        Poller {
            dev: format!("reinsert_{name}_{}", std::process::id()),
            cfg: Arc::new(RwLock::new(Config::default())),
            had_disc: false,
            started: 0,
        }
    }

    // The loop's tick for a drive it probes: dispatch takes the insert claim.
    fn tick(&mut self, probe: libfreemkv::Result<DiscPresence>) {
        let tick = probed_tick(&self.dev, &probe, self.had_disc, &self.cfg, 0, false);
        if let ProbedTick::Answered {
            action: PollAction::Present(t),
            ..
        } = tick
            && t.dispatch
            && state::try_claim_insert(&self.dev).is_some()
        {
            self.started += 1;
        }
        self.had_disc = tick.latch();
    }

    fn present(&mut self) {
        self.tick(Ok(Present));
    }

    // The rip the last insert started stops on a drive fault.
    fn rip_faults(&self) {
        update_state_with(&self.dev, |r| r.status = "error".into());
    }

    // The rip the last insert started finished and delivered.
    fn rip_finishes(&self) {
        update_state_with(&self.dev, |r| r.status = "done".into());
    }

    // An operator Stop that drained the rip (web.rs handle_stop).
    fn stop(&self) {
        hold_stopped_disc(&self.dev);
        update_state_with(&self.dev, |r| r.status = "idle".into());
    }

    fn claim_gen(&self) -> u64 {
        STATE.lock().unwrap().get(&self.dev).unwrap().claim_gen
    }
}

impl Drop for Poller {
    fn drop(&mut self) {
        STATE.lock().unwrap().remove(&self.dev);
        state::forget_device_state(&self.dev);
    }
}

fn medium_changed() -> libfreemkv::Result<DiscPresence> {
    Err(libfreemkv::Error::ScsiError {
        opcode: 0,
        status: 2,
        sense: Some(ScsiSense {
            sense_key: 6,
            asc: 0x28,
            ascq: 0,
        }),
    })
}

fn no_answer() -> libfreemkv::Result<DiscPresence> {
    Err(libfreemkv::Error::ScsiError {
        opcode: 0,
        status: 0xFF,
        sense: None,
    })
}

// Park an idle session on a fake drive so the operator's Eject goes through its handle.
fn hold_fake_drive(dev: &str) -> String {
    let (t, _fake) = libfreemkv::test_util::FakeTransport::new();
    let path = format!("/nonexistent/{dev}");
    session::store_session(
        dev,
        DriveSession {
            drive: libfreemkv::Drive::from_transport(Box::new(t)),
            disc: None,
            scanned: false,
            probed: false,
            tmdb: None,
            device_path: path.clone(),
            key_verdict: None,
            keys: None,
            key_error: None,
            key_reads: Default::default(),
        },
    );
    path
}

#[test]
fn a_medium_change_then_a_good_probe_starts_on_insert_once() {
    let mut p = Poller::new("mc");
    p.present();
    p.rip_faults();
    p.present();
    assert_eq!(p.started, 1, "a resident disc does not restart");

    // The operator ejects and re-inserts between two polls: the drive reports the change.
    p.tick(medium_changed());
    p.present();
    assert_eq!(p.started, 2, "the re-inserted disc starts on_insert");
    p.rip_faults();
    p.present();
    p.present();
    assert_eq!(p.started, 2, "once per medium change");
}

#[test]
fn a_failed_probe_then_a_good_probe_is_a_new_insert() {
    let mut p = Poller::new("fail");
    p.present();
    p.rip_faults();
    // The drive stops answering (USB reset, re-plug, the tray cycling) and comes back.
    p.tick(no_answer());
    p.tick(no_answer());
    p.tick(no_answer());
    p.present();
    assert_eq!(p.started, 2, "the disc is new once the drive answers again");
    p.rip_faults();
    p.present();
    assert_eq!(p.started, 2);
}

#[test]
fn an_eject_after_stop_lets_the_next_insert_start() {
    let mut p = Poller::new("stop");
    p.present();
    p.stop();
    p.present();
    assert_eq!(p.started, 1, "a stopped disc stays stopped");

    let path = hold_fake_drive(&p.dev);
    operator_eject(&path);
    // Re-inserted before any poll saw the drive empty.
    p.present();
    assert_eq!(p.started, 2, "Eject ends the Stop's hold on the disc");
}

#[test]
fn an_eject_after_stop_then_one_empty_poll_lets_the_next_insert_start() {
    let mut p = Poller::new("stop_absent");
    p.present();
    p.stop();
    let path = hold_fake_drive(&p.dev);
    operator_eject(&path);
    p.tick(Ok(DiscPresence::Absent));
    p.present();
    assert_eq!(p.started, 2);
}

// Kept from 1.8: a stopped disc does not start again on its own, even across a probe
// failure; only an Eject or the disc leaving the drive ends the Stop.
#[test]
fn a_stopped_disc_stays_stopped_across_a_failed_probe() {
    let mut p = Poller::new("stop_fail");
    p.present();
    p.stop();
    p.tick(no_answer());
    p.present();
    p.present();
    assert_eq!(p.started, 1);
}

// A drive with a rip in progress never gets a second one, whatever its probe says (the
// loop does not probe a busy drive; this drives the probe anyway, as a claim racing the
// busy check would).
#[test]
fn an_active_rip_is_never_started_twice() {
    let mut p = Poller::new("busy");
    p.present();
    let claim_gen = p.claim_gen();
    p.tick(medium_changed());
    p.present();
    p.tick(no_answer());
    p.tick(Ok(Settling));
    state::note_medium_changed(&p.dev);
    p.present();
    p.present();
    assert_eq!(p.started, 1);
    assert_eq!(p.claim_gen(), claim_gen, "the running rip keeps its claim");
    assert!(is_busy(&p.dev));
}

// A bus glitch on a delivered disc left in the drive must not re-rip it; only a real
// medium change (the drive's 28h, as `disc_presence_probe` reports it) starts it again.
#[test]
fn a_probe_glitch_never_re_rips_a_finished_disc() {
    let mut p = Poller::new("done");
    p.present();
    p.rip_finishes();
    p.tick(no_answer());
    p.present();
    assert_eq!(
        p.started, 1,
        "a finished disc stays finished across a glitch"
    );
    state::note_medium_changed(&p.dev);
    p.present();
    assert_eq!(p.started, 2, "a swapped disc is new");
}

// The poller's wedge warning overwrites the `done` row; the finished insertion must
// still survive further failed probes, and a medium change must still end it.
#[test]
fn a_wedge_row_does_not_unfinish_a_delivered_disc() {
    let mut p = Poller::new("wedge");
    p.present();
    p.rip_finishes();
    assert!(publish_poll_row(
        &p.dev,
        RipState {
            device: p.dev.clone(),
            status: "error".into(),
            disc_present: true,
            last_error: "Drive communication failed repeatedly (E4000). Retrying automatically."
                .into(),
            ..Default::default()
        },
    ));
    assert!(same_insertion(&p.dev, &no_answer(), true));
    assert!(!same_insertion(&p.dev, &medium_changed(), true));
}
