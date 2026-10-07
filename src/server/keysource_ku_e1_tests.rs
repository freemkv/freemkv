use super::*;
use crate::ku_fixture::{K1, VID, bd_image, counting, write_sidecar};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

fn has_k1(calls: &Arc<AtomicUsize>) -> libfreemkv::KeySourceFactory {
    crate::ku_fixture::holding(calls, K1)
}

// KU-E1 invariant: a fresh multipass rip asks the key service ONCE, at the drive scan
// (the drive's set, VID in memory). Its staged-ISO open and mux, handed that set,
// ask no key source again.
#[test]
fn a_fresh_rip_asks_the_key_service_once_at_the_drive_scan() {
    let fx = bd_image();
    let dir = tempfile::tempdir().unwrap();
    let iso = fx.write(dir.path(), "disc.iso");
    write_sidecar(&fx, &iso, false);
    let calls = Arc::new(AtomicUsize::new(0));
    let mut drive_disc = fx.scan();
    drive_disc.aacs.as_mut().unwrap().volume_id = VID;
    let mut drive = libfreemkv::test_util::MemSource::new(fx.img.image.clone());
    let scope = libfreemkv::keys::KeyScope::Titles(vec![0]);
    let set = resolve_with(&drive_disc, &mut drive, scope, &has_k1(&calls), None, None)
        .expect("the drive scan resolves the rip's set");
    let at_scan = calls.load(Ordering::SeqCst);
    assert!(at_scan >= 1, "the scan asked the key service");

    let keys = freemkv_engine::KeyInput::Seeded(has_k1(&calls), set);
    let image = open_staged(&iso, fx.scan(), &[0], keys, None, None).unwrap();
    assert!(image.sources.is_none(), "no source is kept past the open");
    let dest = format!("mkv://{}", dir.path().join("out.mkv").display());
    let out = freemkv_engine::mux_image_titles(
        &image,
        &freemkv_engine::MuxPlan::new(vec![0]),
        &|_| dest.clone(),
        &freemkv_engine::NoopSink,
    );
    assert!(
        matches!(out, freemkv_engine::RipOutcome::Ok { titles_written: 1 }),
        "{out:?}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        at_scan,
        "no second key-service call after the scan"
    );
}

// A resolve that finds no key refuses before any output, never a keyless set.
#[test]
fn a_drive_scan_with_no_key_refuses() {
    let fx = bd_image();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut drive = libfreemkv::test_util::MemSource::new(fx.img.image.clone());
    let scope = libfreemkv::keys::KeyScope::Titles(vec![0]);
    let r = resolve_with(&fx.scan(), &mut drive, scope, &counting(&calls), None, None);
    let e = r.expect_err("no key");
    assert_eq!(e.code(), libfreemkv::error::E_NO_DISC_KEY, "{e}");
}

// A refused resolve still logs its per-source walk: on a refusal it is the operator's only
// view of why a key missed. Checked on the trace, then on the output `resolve_with` logs.
#[test]
fn a_refused_resolve_logs_its_key_walk() {
    let fx = bd_image();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut drive = libfreemkv::test_util::MemSource::new(fx.img.image.clone());
    let scope = libfreemkv::keys::KeyScope::Titles(vec![0]);
    let (set, trace) = freemkv_engine::keys::resolve_for_rip_traced(
        &fx.scan(),
        &mut drive,
        scope,
        &counting(&calls),
        None,
        None,
    );
    assert!(set.is_err(), "no key");
    let walk = render_resolution_trace(&trace, "");
    assert!(
        walk.iter().any(|l| l.contains("online >")),
        "the refused walk renders: {walk:?}"
    );
    // `resolve_with` itself hands the refused walk to the logger.
    use tracing_subscriber::layer::SubscriberExt as _;
    #[derive(Clone, Default)]
    struct Buf(Arc<std::sync::Mutex<Vec<u8>>>);
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
    let mut drive = libfreemkv::test_util::MemSource::new(fx.img.image.clone());
    let scope = libfreemkv::keys::KeyScope::Titles(vec![0]);
    // With this the only live dispatcher, tracing-core decides a callsite's interest from the
    // default of whichever thread first hits it: a concurrent test with no subscriber would
    // cache "never" for the walk's lines. A second live dispatcher makes it ask every one.
    let _second = tracing::Dispatch::new(tracing_subscriber::registry());
    let r = tracing::subscriber::with_default(subscriber, || {
        resolve_with(&fx.scan(), &mut drive, scope, &counting(&calls), None, None)
    });
    assert!(r.is_err(), "no key");
    let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert!(
        out.contains("key_resolve") && out.contains("online >"),
        "the refused walk must be logged by resolve_with: {out}"
    );
}

// An online key service that is down is an outage, never Missing: E7028, even with the
// disc's VID fingerprint on the sidecar (never E7034 "insert the disc"). `localhost` is
// refused at the first query, with no network.
#[test]
fn a_down_key_service_is_e7028_not_missing() {
    let fx = bd_image();
    let dir = tempfile::tempdir().unwrap();
    let iso = fx.write(dir.path(), "disc.iso");
    write_sidecar(&fx, &iso, true);
    let cfg = Config {
        keydb_path: Some(dir.path().join("none.cfg").to_string_lossy().into_owned()),
        key_source: "online".into(),
        keyserver_url: "https://localhost:9/decode".into(),
        ..Config::default()
    };
    let keys = StagedKeys::Resolve { vid: None };
    let Err(e) = open_staged_image(&cfg, &iso, fx.scan(), &[0], keys, None) else {
        panic!("a down key service keys nothing");
    };
    assert_eq!(
        e.code(),
        libfreemkv::error::E_KEY_SERVICE_UNAVAILABLE,
        "{e}"
    );
}

// The ripping process lends its set to the mux worker by ISO path, in memory.
#[test]
fn rip_keys_are_held_per_iso_until_forgotten() {
    let iso = Path::new("/staging/ku-e1-held/disc.iso");
    assert!(rip_keys_for(iso).is_none());
    hold_rip_keys(iso, libfreemkv::keys::KeyRing::none());
    assert!(rip_keys_for(iso).is_some());
    forget_rip_keys(iso);
    assert!(rip_keys_for(iso).is_none());
}

// A raw reader that records every LBA asked of it, failing the ones in `fail`.
struct Recording {
    inner: libfreemkv::test_util::MemSource,
    lbas: Vec<u32>,
    fail: Vec<u32>,
}

impl Recording {
    fn new(image: Vec<u8>) -> Self {
        Recording {
            inner: libfreemkv::test_util::MemSource::new(image),
            lbas: Vec::new(),
            fail: Vec::new(),
        }
    }
}

impl libfreemkv::SectorSource for Recording {
    fn capacity_sectors(&self) -> u32 {
        self.inner.capacity_sectors()
    }
    fn read_sectors(
        &mut self,
        lba: u32,
        count: u16,
        buf: &mut [u8],
        recovery: bool,
    ) -> libfreemkv::error::Result<usize> {
        self.lbas.push(lba);
        if self.fail.contains(&lba) {
            return Err(libfreemkv::Error::DiscRead {
                sector: u64::from(lba),
                status: None,
                sense: None,
            });
        }
        self.inner.read_sectors(lba, count, buf, recovery)
    }
}

// A key retry over the scan's reads re-asks the sources without reading the disc again:
// not its filesystem (the evidence), not its sampled units.
#[test]
fn a_retry_over_the_scans_reads_reads_nothing_from_the_disc() {
    let fx = bd_image();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut drive = Recording::new(fx.img.image.clone());
    let disc = fx.scan();
    let scope = || libfreemkv::keys::KeyScope::Titles(vec![0]);
    let mut reads = KeyReads::default();
    let first = resolve_with(
        &disc,
        &mut reads.over(&mut drive),
        scope(),
        &counting(&calls),
        None,
        None,
    );
    assert!(first.is_err(), "no key");
    let asked = calls.load(Ordering::SeqCst);
    assert!(asked >= 1, "the scan asked the sources");
    // The UDF anchor (ECMA-167 3/8.4.2.1, sector 256) opens every filesystem walk.
    assert!(
        drive.lbas.contains(&256),
        "the scan read the disc: {:?}",
        drive.lbas
    );
    drive.lbas.clear();

    let retry = resolve_with(
        &disc,
        &mut reads.over(&mut drive),
        scope(),
        &counting(&calls),
        None,
        None,
    );
    assert!(retry.is_err(), "still no key");
    assert!(
        calls.load(Ordering::SeqCst) > asked,
        "the retry re-asked the sources"
    );
    assert!(
        drive.lbas.is_empty(),
        "the retry read the disc: {:?}",
        drive.lbas
    );
}

// Only a full read is kept: a failed one is asked of the drive again, and a forced re-fetch
// (FUA) never comes from memory.
#[test]
fn key_reads_keep_only_full_reads() {
    use libfreemkv::SectorSource as _;
    let mut drive = Recording::new(vec![7u8; 16 * 2048]);
    drive.fail = vec![3];
    let mut reads = KeyReads::default();
    let mut buf = vec![0u8; 2048];
    for _ in 0..2 {
        let mut src = reads.over(&mut drive);
        assert_eq!(src.read_sectors(1, 1, &mut buf, false).unwrap(), 2048);
        assert!(src.read_sectors(3, 1, &mut buf, false).is_err());
    }
    assert_eq!(
        drive.lbas,
        [1, 3, 3],
        "the good read once, the failed one each time"
    );
    assert_eq!(buf[0], 7);
    drive.lbas.clear();
    let mut src = reads.over(&mut drive);
    src.read_sectors_fua(1, 1, &mut buf, false, true).unwrap();
    assert_eq!(drive.lbas, [1], "a forced re-fetch reads the drive");
}
