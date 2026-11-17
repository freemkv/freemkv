//! `CachedReads` forwards everything but a repeated full read to the reader behind it.
use super::KeyReads;
use libfreemkv::SectorSource;
use libfreemkv::test_util::MemSource;

#[test]
fn cached_reads_serve_repeats_from_memory_and_forward_the_rest() {
    let mut image = vec![0u8; 8 * 2048];
    image[2048] = 0xAB;
    let mut src = MemSource::new(image);
    let capacity = src.capacity_sectors();
    let random = src.random_access();
    let mut reads = KeyReads::default();
    let mut cached = reads.over(&mut src);
    assert_eq!(cached.capacity_sectors(), capacity);
    assert_eq!(cached.random_access(), random);
    assert!(cached.unmapped_stream_files().is_empty());
    cached.set_speed(0xFFFF);
    cached.set_unit_base(0);
    let mut buf = vec![0u8; 2048];
    assert_eq!(cached.read_sectors(1, 1, &mut buf, false).unwrap(), 2048);
    assert_eq!(buf[0], 0xAB);
    buf.fill(0);
    // Held now: answered from memory.
    assert_eq!(cached.read_sectors(1, 1, &mut buf, false).unwrap(), 2048);
    assert_eq!(buf[0], 0xAB);
    buf.fill(0);
    assert_eq!(
        cached
            .read_sectors_fua(1, 1, &mut buf, false, true)
            .unwrap(),
        2048
    );
    assert_eq!(buf[0], 0xAB);
}
