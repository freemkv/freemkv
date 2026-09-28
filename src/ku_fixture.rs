//! Test fixture for the keys-up-front (KU-E1) server tests: a scannable AACS-encrypted BD
//! image with one playlist per stream file, and a key source that counts its requests.
//!
//! The MPLS/CLPI builders follow freemkv-engine's `test_fixtures` (itself after libfreemkv's
//! crate-private test builders). Each clip is BD LPCM audio in real TS/PES, so a title
//! muxes to a real MKV.
// The sidecar and counting-source helpers serve only the server's tests.
#![cfg_attr(not(feature = "server"), allow(dead_code))]

use libfreemkv::aacs::mkb::AacsVersion;
use libfreemkv::test_util::{BdFile, EncryptedBdImage, MemSource, encrypted_bd_image};
use libfreemkv::{Disc, SectorSource};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

pub(crate) const K1: [u8; 16] = *b"\xA1KU-E1 srv key 1";
pub(crate) const VID: [u8; 16] = *b"\x5AKU-E1 server VD";

// Units per clip: at least `MIN_SAMPLE_UNITS` (8), so a sampled source can be asked.
const CLIP_UNITS: u32 = 10;
const AUDIO_PID: u16 = 0x1100;
const TITLE_SECS: u32 = 120;

// A one-PlayItem MPLS on `clip` whose STN lists the LPCM stream; in/out in 45 kHz ticks.
fn one_item_mpls(clip: &[u8; 5]) -> Vec<u8> {
    let mut item = clip.to_vec();
    item.extend_from_slice(b"M2TS");
    item.extend_from_slice(&[0u8; 3]);
    item.extend_from_slice(&45_000u32.to_be_bytes());
    item.extend_from_slice(&(45_000u32 * (1 + TITLE_SECS)).to_be_bytes());
    item.extend_from_slice(&[0u8; 12]);
    let mut stn = vec![0u8, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    stn.extend_from_slice(&[3, 0x01]);
    stn.extend_from_slice(&AUDIO_PID.to_be_bytes());
    stn.extend_from_slice(&[5, 0x80, 0x31]);
    stn.extend_from_slice(b"eng");
    item.extend_from_slice(&stn);
    let mut pl = vec![0u8; 6];
    pl.extend_from_slice(&1u16.to_be_bytes());
    pl.extend_from_slice(&[0u8; 2]);
    pl.extend_from_slice(&(item.len() as u16).to_be_bytes());
    pl.extend_from_slice(&item);
    let pl_len = (pl.len() - 4) as u32;
    pl[0..4].copy_from_slice(&pl_len.to_be_bytes());
    let mut buf = b"MPLS0200".to_vec();
    buf.extend_from_slice(&40u32.to_be_bytes());
    buf.extend_from_slice(&[0u8; 28]);
    buf.extend_from_slice(&pl);
    buf
}

// Source packet `k` of a clip: TP_extra_header (CPI 11₂ when `encrypted`) and one TS packet
// holding one whole LPCM PES, PTS spread over the title.
fn lpcm_source_packet(k: u32, n: u32, encrypted: bool) -> [u8; 192] {
    const AUDIO: usize = 160;
    let mut p = [0u8; 192];
    p[..4].copy_from_slice(&(k * 100).to_be_bytes());
    p[0] = if encrypted { p[0] | 0xC0 } else { p[0] & 0x3F };
    let ts = &mut p[4..];
    ts[..4].copy_from_slice(&[0x47, 0x40 | (AUDIO_PID >> 8) as u8, AUDIO_PID as u8, 0x30]);
    ts[3] |= (k & 0x0F) as u8;
    let stuffing = 188 - 4 - 1 - (14 + 4 + AUDIO);
    ts[4] = stuffing as u8;
    ts[5] = 0x00;
    ts[6..5 + stuffing].fill(0xFF);
    let pts = 90_000u64 + u64::from(k) * 90_000 * u64::from(TITLE_SECS) / u64::from(n);
    let pes = &mut ts[5 + stuffing..];
    pes[..4].copy_from_slice(&[0, 0, 1, 0xBD]);
    pes[4..6].copy_from_slice(&((8 + 4 + AUDIO) as u16).to_be_bytes());
    pes[6..9].copy_from_slice(&[0x81, 0x80, 5]);
    pes[9] = 0x21 | ((pts >> 29) & 0x0E) as u8;
    pes[10..12].copy_from_slice(&((((pts >> 14) & 0xFFFE) | 1) as u16).to_be_bytes());
    pes[12..14].copy_from_slice(&((((pts << 1) & 0xFFFE) | 1) as u16).to_be_bytes());
    pes[14..16].copy_from_slice(&(AUDIO as u16).to_be_bytes());
    pes[16] = 0x31;
    pes[17] = 0x40;
    for (i, b) in pes[18..18 + AUDIO].iter_mut().enumerate() {
        *b = (k as usize * 7 + i) as u8;
    }
    p
}

// A CLPI with what `clpi::parse` needs: magic and the source packet count at 56.
fn minimal_clpi(source_packets: u32) -> Vec<u8> {
    let mut d = vec![0u8; 60];
    d[0..4].copy_from_slice(b"HDMV");
    d[4..8].copy_from_slice(b"0200");
    d[56..60].copy_from_slice(&source_packets.to_be_bytes());
    d
}

/// A scanned fixture image and the disc its scan found.
pub(crate) struct Fx {
    pub img: EncryptedBdImage,
    pub disc: Disc,
}

impl Fx {
    /// Write the image to `dir/name` and return its path.
    pub(crate) fn write(&self, dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, &self.img.image).unwrap();
        p
    }

    /// Scan the image again: `Disc` is not `Clone`.
    pub(crate) fn scan(&self) -> Disc {
        let mut src = MemSource::new(self.img.image.clone());
        let cap = src.capacity_sectors();
        Disc::scan_image(&mut src, cap, &libfreemkv::ScanOptions::default()).unwrap()
    }
}

/// A BD image with one title whose clip is encrypted with `K1` (one declared CPS unit).
pub(crate) fn bd_image() -> Fx {
    let uk_ro = libfreemkv::test_util::unit_key_ro(AacsVersion::V10, &[[0xEE; 16]], &[1u16]);
    let files = vec![
        BdFile::new("BDMV/index.bdmv", 1, None),
        BdFile::new("BDMV/PLAYLIST/00000.mpls", 1, None),
        BdFile::new("BDMV/CLIPINF/00000.clpi", 1, None),
        BdFile::new("BDMV/STREAM/00000.m2ts", CLIP_UNITS * 3, Some(K1)),
    ];
    let mut img = encrypted_bd_image(&files, &uk_ro);
    let n_packets = CLIP_UNITS * 32;
    let (start, _) = img.files[3];
    for u in 0..CLIP_UNITS {
        let mut unit: Vec<u8> = (0..32)
            .flat_map(|j| lpcm_source_packet(u * 32 + j, n_packets, true))
            .collect();
        let at = (start + u * 3) as usize * 2048;
        img.plain[at..at + unit.len()].copy_from_slice(&unit);
        assert!(libfreemkv::aacs::content::encrypt_unit(&mut unit, &K1));
        img.image[at..at + unit.len()].copy_from_slice(&unit);
    }
    for (f, bytes) in [
        (1, one_item_mpls(b"00000")),
        (2, minimal_clpi(CLIP_UNITS * 32)),
    ] {
        let at = img.files[f].0 as usize * 2048;
        img.image[at..at + bytes.len()].copy_from_slice(&bytes);
    }
    let mut fx = Fx {
        img,
        disc: Disc {
            volume_id: String::new(),
            meta_title: None,
            format: libfreemkv::DiscFormat::BluRay,
            capacity_sectors: 0,
            capacity_bytes: 0,
            layers: 1,
            titles: Vec::new(),
            region: libfreemkv::disc::DiscRegion::Free,
            aacs: None,
            css: None,
            encrypted: false,
            aacs_error: None,
            css_error: None,
            content_format: libfreemkv::ContentFormat::BdTs,
        },
    };
    fx.disc = fx.scan();
    assert_eq!(fx.disc.titles.len(), 1, "the fixture scans to one title");
    fx
}

/// A sidecar mapfile beside `iso`: fully recovered, this disc's hash and, with
/// `vidfp`, a Volume ID fingerprint (all a J6 mapfile holds of the VID).
pub(crate) fn write_sidecar(fx: &Fx, iso: &Path, vidfp: bool) -> PathBuf {
    let path = freemkv_engine::mapfile_path_for(iso);
    let mut map =
        freemkv_engine::Mapfile::create(&path, fx.img.image.len() as u64, "ku-e1").unwrap();
    map.record(
        0,
        fx.img.image.len() as u64,
        freemkv_engine::SectorStatus::Finished,
    )
    .unwrap();
    map.set_disc_hash(&fx.disc.aacs.as_ref().unwrap().disc_hash);
    if vidfp {
        map.set_vid_fingerprint([0x5A; 32]);
    }
    map.flush().unwrap();
    path
}

/// A key source that counts its requests and holds no key.
pub(crate) struct Counting(pub Arc<AtomicUsize>);

impl libfreemkv::KeySource for Counting {
    fn get_unit_keys(
        &self,
        _: &dyn libfreemkv::keysource::ResolveCtx,
    ) -> libfreemkv::Result<Vec<libfreemkv::aacs::types::UnitKey>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(Vec::new())
    }

    fn get_fmts_indexes(
        &self,
        _: &dyn libfreemkv::keysource::ResolveCtx,
    ) -> libfreemkv::Result<Vec<libfreemkv::aacs::types::UnitKey>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(Vec::new())
    }

    fn label(&self) -> &'static str {
        "online"
    }
}

/// A factory of [`Counting`] sources sharing `calls`.
pub(crate) fn counting(calls: &Arc<AtomicUsize>) -> libfreemkv::KeySourceFactory {
    let calls = calls.clone();
    Arc::new(move || vec![Box::new(Counting(calls.clone())) as Box<dyn libfreemkv::KeySource>])
}
