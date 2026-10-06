use super::*;

// A disc-supplied volume-id must not carry terminal escapes into a log (rediscovery's
// `tracing` fields reach autorip.log/stderr unescaped).
#[test]
fn a_volume_id_reaches_a_log_field_with_no_terminal_escapes() {
    // ESC [ 2 J is "clear screen"; a bare CR hides the line before it.
    let hostile = "DISC\x1b[2J\rHARMLESS\x07";
    let logged = vid_for_log(hostile);
    assert!(
        !logged.chars().any(|c| c.is_control()),
        "a disc-supplied volume-id must reach a log field with no control \
             bytes; got {logged:?}"
    );
    assert!(
        logged.contains("DISC") && logged.contains("HARMLESS"),
        "sanitising must not destroy the identifier's readable text; got {logged:?}"
    );
}

// Catches the mutation dropping rollback_failed_spawn's generation check, and the one
// restoring the round-1 rip_thread_running early return.
#[test]
fn rollback_scoped_to_its_own_claim_spares_the_winner_and_clears_the_loser() {
    let dev = format!("rollback-live-worker-test-{}", std::process::id());
    let _ = super::take_rip_thread(&dev);
    let winner_gen = super::super::try_claim_active(&dev).expect("claim must succeed");
    let winner_halt = Halt::new();
    super::super::register_halt(&dev, winner_halt);

    // A worker that is still on the CPU — the incumbent/winner.
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    super::spawn_rip_thread(&dev, "rip", move || {
        let _ = release_rx.recv();
    })
    .expect("the first spawn owns the device");

    // The loser rolls back a claim that is NO LONGER in force (the winner's
    // generation is the current one). Nothing of the winner's may move.
    let stale_gen = winner_gen.saturating_sub(1);
    super::rollback_failed_spawn(&dev, stale_gen);
    assert!(
        super::super::device_halt(&dev).is_some(),
        "a rollback for a superseded claim must NOT unregister the live \
             worker's Halt — /api/stop would then have no token with which to \
             cancel the running rip"
    );
    assert_eq!(
        super::super::STATE
            .lock()
            .unwrap()
            .get(&dev)
            .map(|r| r.status.clone()),
        Some("scanning".to_string()),
        "a rollback for a superseded claim must NOT idle the winner"
    );

    // The H2 wedge: the claim IN FORCE is rolled back while a worker is
    // still alive — the shape of a losing `/api/rip` whose spawn came back
    // `PriorThreadRunning`. Round 1 returned early here, wedging "scanning".
    super::super::register_halt(&dev, Halt::new());
    super::rollback_failed_spawn(&dev, winner_gen);
    assert!(
        super::super::device_halt(&dev).is_none(),
        "rolling back the claim in force must also release the Halt that \
             claim registered, or the next rip inherits a stale token"
    );
    assert_eq!(
        super::super::STATE
            .lock()
            .unwrap()
            .get(&dev)
            .map(|r| r.status.clone()),
        Some("idle".to_string()),
        "rolling back the claim that is in force must clear it — leaving it \
             set wedges every route on this device at 409 with no thread and no \
             Halt to recover from"
    );

    drop(release_tx);
    let _ = super::join_rip_thread(&dev, std::time::Duration::from_secs(10));
    super::super::STATE.lock().unwrap().remove(&dev);
    let _ = super::take_rip_thread(&dev);
    super::super::unregister_halt(&dev);
}

#[test]
fn rollback_failed_spawn_clears_halt_and_idles() {
    let dev = format!("rollback-test-{}", std::process::id());
    // Simulate the pre-spawn state: claim (sets status=scanning) +
    // register a Halt, exactly as the poll loop / web handlers do.
    let claim_gen = super::super::try_claim_active(&dev).expect("claim must succeed");
    super::super::register_halt(&dev, Halt::new());
    assert!(super::super::device_halt(&dev).is_some(), "halt registered");

    super::super::rollback_failed_spawn(&dev, claim_gen);

    // Halt is gone (no leak) and the device is idle with the disc still
    // present, so a future scan/rip is not wedged at 409.
    assert!(
        super::super::device_halt(&dev).is_none(),
        "rollback must unregister the halt"
    );
    let snap = super::super::STATE
        .lock()
        .unwrap()
        .get(&dev)
        .cloned()
        .expect("state entry exists");
    assert_eq!(snap.status, "idle", "must roll back to idle");
    assert!(snap.disc_present, "disc still present after rollback");
    assert!(!super::super::is_busy(&dev), "device no longer busy");
    // The map lookup above only proves the KEY is right, not that the
    // struct's own `device` field in rollback's RipState literal survived.
    assert_eq!(
        snap.device, dev,
        "device field in the rollback RipState must match"
    );
}

#[test]
fn swap_halt_carrying_cancel_carries_forward_a_pending_cancel() {
    // HIGH-ish (rule 3, TOCTOU): swap_halt_carrying_cancel exists so a Stop
    // landing on the OUTGOING placeholder between allocation and swap isn't
    // lost. Pin both: cancelled outgoing carries forward; clean one doesn't.
    let dev = format!("swap-halt-test-{}", std::process::id());

    // Case 1: outgoing token already cancelled (a Stop raced the swap).
    let placeholder = Halt::new();
    super::super::register_halt(&dev, placeholder.clone());
    placeholder.cancel();
    let real = Halt::new();
    super::super::swap_halt_carrying_cancel(&dev, real.clone());
    assert!(
        real.is_cancelled(),
        "a Stop that landed on the outgoing placeholder must carry \
             forward onto the freshly-swapped-in token, or the drain hangs \
             waiting for a Halt nobody ever cancels"
    );

    // Case 2: outgoing token NOT cancelled — the new token must stay
    // live, not get spuriously cancelled by the swap itself.
    let dev2 = format!("swap-halt-test-clean-{}", std::process::id());
    let placeholder2 = Halt::new();
    super::super::register_halt(&dev2, placeholder2);
    let real2 = Halt::new();
    super::super::swap_halt_carrying_cancel(&dev2, real2.clone());
    assert!(
        !real2.is_cancelled(),
        "swapping in a new Halt must not cancel it when nothing asked \
             to stop"
    );
}

#[test]
fn path_unchanged_only_for_zero_delta() {
    // delta==0 skips disc-identity verification entirely (same device
    // node => trusted by construction); any nonzero delta is a SHIFTED
    // candidate and must go through the identity check instead.
    assert!(path_unchanged(0));
    assert!(!path_unchanged(1));
    assert!(!path_unchanged(-1));
    assert!(!path_unchanged(3));
}

#[test]
fn candidate_identity_confirmed_requires_exact_match() {
    // A shifted-sg candidate is accepted only if its probed volume id
    // EXACTLY matches expected; a failed probe or a different disc's id
    // must be rejected, or a neighbour's disc could hijack the rip session.
    assert!(candidate_identity_confirmed(
        Some("DISC_VOL_123"),
        "DISC_VOL_123"
    ));
    assert!(!candidate_identity_confirmed(
        Some("SOME_OTHER_DISC"),
        "DISC_VOL_123"
    ));
    assert!(!candidate_identity_confirmed(None, "DISC_VOL_123"));
}

// Swapping in a disc with NO volume label must not leave the previous disc's identity
// cached (the old `filter`-and-skip form left it stale).
#[test]
fn an_unlabelled_disc_clears_the_previous_discs_cached_identity() {
    let dev = format!("disc-identity-swap-{}", std::process::id());

    cache_disc_identity(&dev, "FIRST_DISC_VOL");
    assert_eq!(
        expected_volume_id(&dev).as_deref(),
        Some("FIRST_DISC_VOL"),
        "a labelled disc is cached"
    );

    // Operator swaps in a disc with no UDF volume label.
    cache_disc_identity(&dev, "");
    assert_eq!(
        expected_volume_id(&dev),
        None,
        "an unlabelled disc must leave NO identity, not the ejected \
             disc's — verifying a rediscovery candidate against a disc that \
             is no longer in the drive is how the wrong disc gets attached"
    );

    // Whitespace-only is the same non-label (the label is trimmed).
    cache_disc_identity(&dev, "SECOND_DISC_VOL");
    cache_disc_identity(&dev, "   ");
    assert_eq!(expected_volume_id(&dev), None);
}

// Regression: hot-unplug teardown must not leak this module's per-device maps
// (RIP_THREADS/DISC_IDENTITY/HALTS), as it used to.
#[test]
fn forgetting_a_removed_device_reaps_its_finished_thread_and_identity() {
    // Fixture name unique to this test: RIP_THREADS / DISC_IDENTITY /
    // HALTS are process-global and shared across the whole test binary.
    let dev = format!("forget-reap-{}", std::process::id());

    // A worker that exits immediately, registered exactly as the poll
    // loop registers a real rip thread.
    spawn_rip_thread(&dev, "rip", || {}).expect("spawn must succeed");
    register_halt(&dev, Halt::new());
    DISC_IDENTITY
        .lock()
        .unwrap()
        .insert(dev.clone(), "VOL_FORGET_REAP".to_string());

    // Watchdog: wait for the worker to exit before asserting the reap,
    // without blocking the suite forever. 5s is ample margin for an
    // empty closure's spawn+exit; a hung regression fails here instead.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let finished = RIP_THREADS
            .lock()
            .unwrap()
            .get(&dev)
            .is_some_and(|h| h.is_finished());
        if finished {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "worker thread did not exit within 5s"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    super::super::state::forget_device_state(&dev);

    assert!(
        !RIP_THREADS.lock().unwrap().contains_key(&dev),
        "a finished JoinHandle must be reaped when the device is torn \
             down, not left in RIP_THREADS forever"
    );
    assert!(
        !DISC_IDENTITY.lock().unwrap().contains_key(&dev),
        "the cached disc identity must be dropped when the device is \
             torn down — nothing else ever removes from DISC_IDENTITY"
    );
    assert!(
        super::super::device_halt(&dev).is_none(),
        "the halt token of an exited thread must go with its handle"
    );
}

// The other half of the contract: a still-RUNNING rip thread must keep its registration, or
// a later drain returns while it is mid-write.
#[test]
fn forgetting_a_device_leaves_a_still_running_thread_registered() {
    let dev = format!("forget-keep-running-{}", std::process::id());
    let gate = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker_gate = gate.clone();
    spawn_rip_thread(&dev, "rip", move || {
        // Watchdog: 5 s is the ceiling, not the expectation — release
        // comes almost immediately. This only stops a regression from
        // parking this thread for the life of the suite.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !worker_gate.load(std::sync::atomic::Ordering::SeqCst)
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
    })
    .expect("spawn must succeed");
    register_halt(&dev, Halt::new());

    super::super::state::forget_device_state(&dev);

    assert!(
        RIP_THREADS.lock().unwrap().contains_key(&dev),
        "a still-running rip thread's handle must survive teardown — \
             dropping it makes the thread unjoinable and breaks \
             drain-before-wipe"
    );
    assert!(
        super::super::device_halt(&dev).is_some(),
        "a still-running thread's Halt must stay reachable so /api/stop \
             can still cancel it"
    );

    gate.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = join_rip_thread(&dev, Duration::from_secs(5));
}

// Regression: take_session/drop_session must recover from a poisoned SESSIONS lock, not
// silently no-op as the old `.lock().ok()?` form did.
#[test]
fn session_helpers_recover_from_poison() {
    // Poison SESSIONS by panicking while the guard is held.
    let _ = std::panic::catch_unwind(|| {
        let _guard = SESSIONS.lock().unwrap();
        panic!("intentional poison");
    });
    assert!(SESSIONS.is_poisoned(), "lock must be poisoned for the test");

    let dev = format!("poison-test-{}", std::process::id());
    // Neither helper may panic on the poisoned lock; both must run.
    assert!(
        take_session(&dev).is_none(),
        "take_session on poisoned lock returns None for an absent device, not panic"
    );
    drop_session(&dev); // must not panic
    // session_is_scanned must also recover, not abandon (the old `.ok()`
    // form would wedge it at `false` forever for the rest of the binary).
    assert!(
        !session_is_scanned(&dev),
        "session_is_scanned on poisoned lock must recover and answer, not panic"
    );
}

// Catches the mutation deleting join_rip_thread's self-join branch: it runs ON its own
// thread from eject_drive, where is_finished() can never become true.
#[test]
fn join_rip_thread_called_on_its_own_thread_returns_at_once() {
    let dev = format!("self-join-test-{}", std::process::id());
    let _ = super::take_rip_thread(&dev);
    let (tx, rx) = std::sync::mpsc::channel::<(std::time::Duration, bool, bool)>();
    let dev_inner = dev.clone();
    super::spawn_rip_thread(&dev, "rip", move || {
        let t0 = std::time::Instant::now();
        // A budget far longer than any test may block for: if the
        // self-join branch is gone this sleeps for all 30 s.
        let outcome = super::join_rip_thread(&dev_inner, std::time::Duration::from_secs(30));
        let still_registered = super::rip_thread_running(&dev_inner);
        let _ = tx.send((t0.elapsed(), outcome.is_ok(), still_registered));
    })
    .expect("spawn");

    let (elapsed, ok, still_registered) = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("a self-join must return immediately, not sit out its timeout");
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "self-join returned after {elapsed:?} — it must not poll its own thread"
    );
    assert!(ok, "a self-join is not a drain failure");
    assert!(
        still_registered,
        "a self-join must leave the handle registered — it is what keeps the \
             device unclaimable for the rest of the worker's tail"
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while super::rip_thread_running(&dev) {
        assert!(std::time::Instant::now() < deadline, "worker should exit");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let _ = super::join_rip_thread(&dev, std::time::Duration::from_secs(5));
}
