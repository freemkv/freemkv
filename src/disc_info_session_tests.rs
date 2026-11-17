//! `report_session` over a fake drive serving a scanned BD image: every report shape runs.
use super::report_session;
use crate::ku_fixtures::{Fx, bd_image};
use crate::output::Output;
use libfreemkv::test_util::FakeTransport;

fn session(fx: &Fx) -> libfreemkv::DiscSession {
    let (t, _fake) = FakeTransport::new();
    let drive = libfreemkv::Drive::from_transport(Box::new(t.with_image(fx.img.image.clone())));
    libfreemkv::DiscSession::from_drive(drive)
}

#[test]
fn a_clear_disc_reports_in_every_listing_mode() {
    let fx = bd_image(&[None, None], 1);
    for (full, basic) in [(false, false), (true, false), (false, true)] {
        let out = Output::new(false, false);
        report_session(session(&fx), &out, false, full, basic, &None);
    }
}

#[test]
fn a_verbose_report_on_an_encrypted_disc_runs_the_key_block() {
    let fx = bd_image(&[Some([0x11; 16]), None], 1);
    let tmp = tempfile::tempdir().unwrap();
    let keydb = Some(tmp.path().join("no-keydb.cfg").display().to_string());
    let out = Output::new(true, false);
    report_session(session(&fx), &out, true, false, false, &keydb);
    let out = Output::new(false, true);
    report_session(session(&fx), &out, false, true, false, &None);
}
