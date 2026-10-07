use super::{KeyRefusal, KeyResult, key_refusal, resolve_rip_keys, retry_keys_with};
use crate::server::keysource::ServiceReachability as R;
use std::cell::Cell;
use std::sync::{Arc, Mutex};

// The tracing output of `f`, for the "definitive verdict" line the classifier must not log
// for a refusal the key service never gave.
fn traced<T>(f: impl FnOnce() -> T) -> (T, String) {
    use tracing_subscriber::layer::SubscriberExt as _;
    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buf {
        type Writer = Buf;
        fn make_writer(&'a self) -> Buf {
            self.clone()
        }
    }
    let buf = Buf::default();
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .with_writer(buf.clone())
            .with_ansi(false),
    );
    // A second live dispatcher: tracing-core then asks every one for a callsite's interest,
    // not the default of whichever (subscriber-less) test thread hit it first.
    let _second = tracing::Dispatch::new(tracing_subscriber::registry());
    let out = tracing::subscriber::with_default(subscriber, f);
    let text = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    (out, text)
}

const VERDICT_LINE: &str = "definitive verdict";

// What the classifier asked of its effects.
#[derive(Default)]
struct Calls {
    attempts: Cell<u32>,
    probes: Cell<u32>,
    waits: Cell<u32>,
}

// `retry_keys_with` over scripted effects: each attempt pops the next of `attempts`, the
// probe answers `probe`, every wait passes.
fn run(
    device: &str,
    refused: libfreemkv::Error,
    decode_reach: Option<R>,
    attempts: Vec<(KeyResult, Option<R>)>,
    probe: R,
    calls: &Calls,
) -> (KeyResult, Option<R>) {
    let mut attempts = attempts.into_iter();
    retry_keys_with(
        device,
        refused,
        decode_reach,
        || {
            calls.attempts.set(calls.attempts.get() + 1);
            attempts.next().expect("no more scripted attempts")
        },
        &|| {
            calls.probes.set(calls.probes.get() + 1);
            probe
        },
        |_| {
            calls.waits.set(calls.waits.get() + 1);
            true
        },
    )
}

fn disc_read_failure() -> libfreemkv::Error {
    libfreemkv::Error::DiscRead {
        sector: 4106,
        status: None,
        sense: None,
    }
}

fn device(tag: &str) -> String {
    format!("key_retry_{tag}_{}", std::process::id())
}

fn device_log(dev: &str) -> String {
    crate::server::log::get_device_log(dev, 50).join("\n")
}

// Each refusal is classified by the error itself: a Stop, the disc/drive/transport failing,
// or an outcome of the key sources.
#[test]
fn a_refusal_is_classified_by_its_error() {
    let halted_io = libfreemkv::Error::IoError {
        source: libfreemkv::Error::Halted.into(),
    };
    let cases = [
        (libfreemkv::Error::Halted, KeyRefusal::Stopped),
        (halted_io, KeyRefusal::Stopped),
        (disc_read_failure(), KeyRefusal::NotKeyService),
        (
            libfreemkv::Error::IoError {
                source: std::io::Error::other("EIO"),
            },
            KeyRefusal::NotKeyService,
        ),
        (
            libfreemkv::Error::SourceTerminated,
            KeyRefusal::NotKeyService,
        ),
        (
            libfreemkv::Error::KeyServiceUnavailable,
            KeyRefusal::KeyService,
        ),
        (
            libfreemkv::Error::KeyServiceRateLimited,
            KeyRefusal::KeyService,
        ),
        (
            libfreemkv::Error::WholeDiscKeyMissing,
            KeyRefusal::KeyService,
        ),
        (libfreemkv::Error::FmtsKeyMissing, KeyRefusal::KeyService),
        (
            libfreemkv::Error::NoDiscKey {
                disc_hash: "0xabc".into(),
            },
            KeyRefusal::KeyService,
        ),
    ];
    for (e, want) in cases {
        assert_eq!(key_refusal(&e), want, "{e}");
    }
}

// A Stop (E6010) during the key resolution is a Stop: no reachability probe, no verdict
// logged, no retry.
#[test]
fn a_stop_is_not_a_key_service_verdict() {
    let dev = device("stop");
    let calls = Calls::default();
    let ((keys, verdict), log) = traced(|| {
        run(
            &dev,
            libfreemkv::Error::Halted,
            None,
            Vec::new(),
            R::Answered,
            &calls,
        )
    });
    assert!(matches!(keys, Err(libfreemkv::Error::Halted)));
    assert_eq!(verdict, None);
    assert_eq!(calls.probes.get(), 0, "a Stop sends no probe");
    assert_eq!(
        calls.attempts.get() + calls.waits.get(),
        0,
        "a Stop is not retried"
    );
    assert!(!log.contains(VERDICT_LINE), "{log}");
    assert!(device_log(&dev).contains("Stopped during key resolution."));
}

// A disc read failure (E6000) is not the key service's answer, even with a decode verdict
// in hand: no empty probe, no verdict, no retry; the error itself is what the rip reports.
#[test]
fn a_disc_read_failure_is_not_a_key_service_verdict() {
    for decode in [None, Some(R::Unreachable)] {
        let dev = device("disc_read");
        let calls = Calls::default();
        let ((keys, verdict), log) = traced(|| {
            run(
                &dev,
                disc_read_failure(),
                decode,
                Vec::new(),
                R::Answered,
                &calls,
            )
        });
        assert!(
            matches!(keys, Err(libfreemkv::Error::DiscRead { .. })),
            "{decode:?}"
        );
        assert_eq!(verdict, None, "{decode:?}");
        assert_eq!(calls.probes.get(), 0, "no empty probe for a read failure");
        assert_eq!(calls.attempts.get() + calls.waits.get(), 0, "not retried");
        assert!(!log.contains(VERDICT_LINE), "{log}");
        let dlog = device_log(&dev);
        assert!(dlog.contains("not at the key service"), "{dlog}");
        assert!(!dlog.contains("appears DOWN"), "{dlog}");
    }
}

// A key-service outcome is classified by reachability: from the real decode's answer, and
// only with no decode answer by the probe.
#[test]
fn a_key_service_outcome_is_classified_by_reachability() {
    let calls = Calls::default();
    let ((_, verdict), log) = traced(|| {
        run(
            &device("answered"),
            libfreemkv::Error::KeyServiceUnavailable,
            None,
            Vec::new(),
            R::Answered,
            &calls,
        )
    });
    assert_eq!(verdict, Some(R::Answered));
    assert_eq!(calls.probes.get(), 1, "no decode answer: probe once");
    assert_eq!(calls.attempts.get(), 0, "an answer is not retried");
    assert!(log.contains(VERDICT_LINE), "{log}");

    let calls = Calls::default();
    let (_, verdict) = run(
        &device("decoded"),
        libfreemkv::Error::NoDiscKey {
            disc_hash: "0xabc".into(),
        },
        Some(R::NoKeyForDisc),
        Vec::new(),
        R::Unreachable,
        &calls,
    );
    assert_eq!(
        verdict,
        Some(R::NoKeyForDisc),
        "the decode's own answer wins"
    );
    assert_eq!(calls.probes.get(), 0);
}

// An outage retries; a retry that fails reading the disc ends the retries with no verdict,
// and a Stop during a retry keeps the outage verdict (as a Stop during the wait does).
#[test]
fn a_retry_ends_on_a_stop_or_a_disc_failure() {
    let calls = Calls::default();
    let (keys, verdict) = run(
        &device("retry_read"),
        libfreemkv::Error::KeyServiceUnavailable,
        Some(R::Unreachable),
        vec![(Err(disc_read_failure()), None)],
        R::Answered,
        &calls,
    );
    assert!(matches!(keys, Err(libfreemkv::Error::DiscRead { .. })));
    assert_eq!(verdict, None);
    assert_eq!((calls.attempts.get(), calls.probes.get()), (1, 0));

    let calls = Calls::default();
    let (keys, verdict) = run(
        &device("retry_stop"),
        libfreemkv::Error::KeyServiceUnavailable,
        Some(R::Unreachable),
        vec![(Err(libfreemkv::Error::Halted), None)],
        R::Answered,
        &calls,
    );
    assert!(matches!(keys, Err(libfreemkv::Error::Halted)));
    assert_eq!(verdict, Some(R::Unreachable));
    assert_eq!((calls.attempts.get(), calls.probes.get()), (1, 0));

    // Still down, then the service answers with no key: classified from each retry's own
    // decode, the probe only when that decode made no HTTP call.
    let calls = Calls::default();
    let (_, verdict) = run(
        &device("retry_answer"),
        libfreemkv::Error::KeyServiceUnavailable,
        Some(R::Unreachable),
        vec![
            (
                Err(libfreemkv::Error::KeyServiceUnavailable),
                Some(R::ServerError(503)),
            ),
            (Err(libfreemkv::Error::KeyServiceUnavailable), None),
        ],
        R::Answered,
        &calls,
    );
    assert_eq!(verdict, Some(R::Answered));
    assert_eq!(
        (calls.attempts.get(), calls.waits.get(), calls.probes.get()),
        (2, 2, 1)
    );
}

// A retry rides the scan's key reads: the second resolve on the live drive re-asks the
// sources without sending the drive a single READ — no filesystem walk (the evidence the
// engine rebuilds), no sampled unit.
#[test]
fn a_key_retry_on_the_drive_reads_nothing_from_the_disc() {
    use libfreemkv::test_util::FakeTransport;
    let fx = crate::ku_fixture::bd_image();
    let (t, fake) = FakeTransport::new();
    let mut drive = libfreemkv::Drive::from_transport(Box::new(t.with_image(fx.img.image.clone())));
    let disc = fx.scan();
    let dir = tempfile::tempdir().unwrap();
    let cfg = crate::server::config::Config {
        keydb_path: Some(dir.path().join("none.cfg").to_string_lossy().into_owned()),
        ..Default::default()
    };
    let read10 = |c: &[u8]| c[0] == 0x28;
    // READ(10) of the UDF anchor (ECMA-167 3/8.4.2.1, sector 256): a filesystem walk.
    let anchor = |c: &[u8]| read10(c) && u32::from_be_bytes([c[2], c[3], c[4], c[5]]) == 256;
    let scope = libfreemkv::keys::KeyScope::Titles(vec![0]);
    let dev = device("reads");
    let mut reads = super::KeyReads::default();
    let first = resolve_rip_keys(&dev, &cfg, &mut drive, &disc, &scope, None, &mut reads);
    assert!(first.is_err(), "no keydb, no key");
    assert!(
        fake.count(anchor) >= 1,
        "the scan reads the filesystem: {:02x?}",
        fake.cdbs()
    );
    let scan_reads = fake.count(read10);
    let retry = resolve_rip_keys(&dev, &cfg, &mut drive, &disc, &scope, None, &mut reads);
    assert!(retry.is_err());
    assert_eq!(
        fake.count(read10),
        scan_reads,
        "the retry must not read the disc again"
    );
}

// Wiring guard: the scan banks its key reads on the session, and the rip's resolves and
// both outage retries ride them.
#[test]
fn the_scans_key_reads_reach_every_retry() {
    let src = crate::server::util::source_lf(include_str!("mod.rs"));
    assert_eq!(
        src.matches("            key_reads,\n").count(),
        2,
        "both scans bank them"
    );
    assert!(src.contains("let mut key_reads = std::mem::take(&mut session.key_reads);"));
    let retries = src.matches("retry_online_keys_on_outage(\n").count()
        - src.matches("fn retry_online_keys_on_outage(\n").count();
    let with_reads = src
        .split("retry_online_keys_on_outage(\n")
        .skip(1)
        .filter(|call| {
            call.split(')')
                .next()
                .is_some_and(|args| args.contains("&mut key_reads"))
        })
        .count();
    assert!(retries >= 2, "both outage retries are calls: {retries}");
    assert_eq!(
        with_reads, retries,
        "every outage retry rides the scan's reads"
    );
}
