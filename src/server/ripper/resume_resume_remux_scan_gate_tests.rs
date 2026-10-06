//! Drive `resume_remux` through the real `libfreemkv::scan_iso` seam with a
//! synthetic minimal UDF image (an empty root directory, so `scan_iso`
//! SUCCEEDS but the disc carries zero titles). That exercises the
//! title-gate abort branch — `scan_iso` ok, `title_ok == false` — which no
//! prior test reached (the archive test aborts one step earlier, on a
//! non-openable ISO). The sector builders are ported verbatim from
//! libfreemkv's `tests/scan_iso.rs` fixture.
use super::*;
use std::collections::BTreeMap;

const SECTOR_SIZE: usize = 2048;

fn make_avdp_sector(vds_lba: u32) -> Vec<u8> {
    let mut s = vec![0u8; SECTOR_SIZE];
    s[0..2].copy_from_slice(&2u16.to_le_bytes());
    s[16..20].copy_from_slice(&vds_lba.to_le_bytes());
    s[20..24].copy_from_slice(&(6u32 * SECTOR_SIZE as u32).to_le_bytes());
    s
}
fn make_pvd_sector(volume_id: &str) -> Vec<u8> {
    let mut s = vec![0u8; SECTOR_SIZE];
    s[0..2].copy_from_slice(&1u16.to_le_bytes());
    if !volume_id.is_empty() {
        let id_bytes = volume_id.as_bytes();
        s[24] = 8;
        let copy_len = id_bytes.len().min(30);
        s[25..25 + copy_len].copy_from_slice(&id_bytes[..copy_len]);
        s[55] = (1 + copy_len) as u8;
    }
    s
}
fn make_partition_desc(partition_start: u32) -> Vec<u8> {
    let mut s = vec![0u8; SECTOR_SIZE];
    s[0..2].copy_from_slice(&5u16.to_le_bytes());
    s[188..192].copy_from_slice(&partition_start.to_le_bytes());
    s
}
fn make_lvd_sector_simple() -> Vec<u8> {
    let mut s = vec![0u8; SECTOR_SIZE];
    s[0..2].copy_from_slice(&6u16.to_le_bytes());
    s[268..272].copy_from_slice(&1u32.to_le_bytes());
    s
}
fn make_terminator() -> Vec<u8> {
    let mut s = vec![0u8; SECTOR_SIZE];
    s[0..2].copy_from_slice(&8u16.to_le_bytes());
    s
}
fn make_fsd_sector(root_meta_lba: u32) -> Vec<u8> {
    let mut s = vec![0u8; SECTOR_SIZE];
    s[0..2].copy_from_slice(&256u16.to_le_bytes());
    s[400..404].copy_from_slice(&(SECTOR_SIZE as u32).to_le_bytes());
    s[404..408].copy_from_slice(&root_meta_lba.to_le_bytes());
    s
}
fn make_dir_icb(data_meta_lba: u32, data_len: u32) -> Vec<u8> {
    let mut s = vec![0u8; SECTOR_SIZE];
    s[0..2].copy_from_slice(&266u16.to_le_bytes());
    s[56..64].copy_from_slice(&(data_len as u64).to_le_bytes());
    s[208..212].copy_from_slice(&0u32.to_le_bytes());
    s[212..216].copy_from_slice(&8u32.to_le_bytes());
    s[216..220].copy_from_slice(&data_len.to_le_bytes());
    s[220..224].copy_from_slice(&data_meta_lba.to_le_bytes());
    s
}
fn make_parent_fid() -> Vec<u8> {
    let fid_len = (38 + 3) & !3;
    let mut fid = vec![0u8; fid_len];
    fid[0..2].copy_from_slice(&257u16.to_le_bytes());
    fid[18] = 0x08;
    fid[19] = 0;
    fid
}
fn minimal_udf_sectors() -> BTreeMap<u32, Vec<u8>> {
    let partition_start: u32 = 512;
    let mut sectors: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
    sectors.insert(256, make_avdp_sector(32));
    sectors.insert(32, make_pvd_sector("TEST_DISC"));
    sectors.insert(33, make_partition_desc(partition_start));
    sectors.insert(34, make_lvd_sector_simple());
    sectors.insert(35, make_terminator());
    sectors.insert(partition_start, make_fsd_sector(1));
    let parent_fid = make_parent_fid();
    let dir_data_len = parent_fid.len() as u32;
    sectors.insert(partition_start + 1, make_dir_icb(2, dir_data_len));
    let mut sector = vec![0u8; SECTOR_SIZE];
    sector[..parent_fid.len()].copy_from_slice(&parent_fid);
    sectors.insert(partition_start + 2, sector);
    sectors
}
fn write_iso(path: &std::path::Path, sectors: &BTreeMap<u32, Vec<u8>>) {
    let max_lba = *sectors.keys().max().unwrap();
    let mut image = vec![0u8; (max_lba as usize + 1) * SECTOR_SIZE];
    for (&lba, data) in sectors {
        let off = lba as usize * SECTOR_SIZE;
        image[off..off + SECTOR_SIZE].copy_from_slice(data);
    }
    std::fs::write(path, &image).expect("write iso");
}

fn tmpdir() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("test-scratch")
        .join(format!(
            "autorip-resume-scangate-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed),
        ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(p.join("logs")).unwrap();
    p
}

// A minimal UDF ISO scans cleanly but carries no titles: resume_remux
// must log the "no usable title" abort and reset scanning -> idle so
// handle_rip's "already ripping" gate doesn't wedge later /api/rip.
#[test]
fn resume_remux_aborts_when_scanned_iso_has_no_usable_title() {
    let _guard = crate::server::log::env_guard();
    let d = tmpdir();
    // SAFETY: env access in tests, serialized by env_guard; the assertions
    // read the in-memory STATE + device-log ring, not the env-routed file.
    unsafe {
        std::env::set_var("AUTORIP_DIR", &d);
    }

    let staging = d.join("Some_Disc_2024");
    std::fs::create_dir_all(&staging).unwrap();
    let iso_path = staging.join("Some_Disc_2024.iso");
    write_iso(&iso_path, &minimal_udf_sectors());

    let dev = format!("test_resume_scangate_sg_{}", std::process::id());
    // The live dispatch would already have moved this device to "scanning";
    // seed that so the abort's scanning→idle reset is observable.
    crate::server::ripper::update_state(
        &dev,
        crate::server::ripper::RipState {
            device: dev.clone(),
            status: "scanning".to_string(),
            disc_present: true,
            ..Default::default()
        },
    );

    let class = ResumeClass::Remux {
        iso_path: iso_path.clone(),
        mapfile_path: staging.join("Some_Disc_2024.iso.mapfile"),
        display_name: "Some_Disc_2024".to_string(),
        title_confident: None,
    };
    let cfg = Arc::new(RwLock::new(Config::default()));

    resume_remux(&cfg, &dev, class);

    let live = crate::server::log::get_device_log(&dev, 200);
    assert!(
        live.iter().any(|l| l.contains("no usable title")),
        "the title-gate abort line must be logged, got: {live:?}"
    );
    let status = crate::server::ripper::STATE
        .lock()
        .unwrap()
        .get(&dev)
        .map(|s| s.status.clone());
    assert_eq!(
        status.as_deref(),
        Some("idle"),
        "a title-gate abort must reset scanning → idle so the rip gate unwedges"
    );

    crate::server::ripper::STATE.lock().unwrap().remove(&dev);
    let _ = std::fs::remove_dir_all(&d);
}
