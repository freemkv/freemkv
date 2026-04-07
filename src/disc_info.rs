// freemkv disc-info — Show disc titles, streams, and sizes
// AGPL-3.0 — freemkv project

use crate::strings;

/// Thin wrapper around DriveSession for our read functions
struct DiscReader<'a>(&'a mut libfreemkv::DriveSession);

impl<'a> DiscReader<'a> {
    fn read_sector(&mut self, lba: u32) -> Result<Vec<u8>, String> {
        let mut buf = vec![0u8; 2048];
        self.0.read_disc(lba, 1, &mut buf)
            .map_err(|e| format!("read sector {}: {}", lba, e))?;
        Ok(buf)
    }

    fn read_capacity(&mut self) -> Option<u32> {
        let cdb = [0x25, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        let mut buf = [0u8; 8];
        self.0.scsi_execute(&cdb, libfreemkv::scsi::DataDirection::FromDevice, &mut buf, 5000).ok()?;
        Some(u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]))
    }
}

pub fn run(args: &[String]) {
    strings::init();

    let mut device_path: Option<String> = None;
    let mut quiet = false;
    let mut full = false;
    let mut basic = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--device" | "-d" => {
                i += 1;
                device_path = args.get(i).cloned();
            }
            "--quiet" | "-q" => quiet = true,
            "--full" | "-f" => full = true,
            "--basic" | "-b" => basic = true,
            "--help" | "-h" => {
                println!("freemkv disc-info — {}", strings::get("disc.scanning"));
                println!();
                println!("Usage: freemkv disc-info [--device /dev/sgN]");
                return;
            }
            _ => {
                eprintln!("Unknown option: {}", args[i]);
                std::process::exit(1);
            }
        }
        i += 1;
    }

    // Auto-detect device
    let dev_path = device_path.unwrap_or_else(|| find_bd_drive().unwrap_or_else(|| {
        eprintln!("{}", strings::get("error.no_drive"));
        std::process::exit(1);
    }));

    if !quiet {
        println!("freemkv {}", env!("CARGO_PKG_VERSION"));
        println!();
        println!("{}", strings::get("disc.scanning"));
        println!();
    }

    // Open drive via libfreemkv — unlocks automatically
    let mut session = match libfreemkv::DriveSession::open(std::path::Path::new(&dev_path)) {
        Ok(s) => s,
        Err(e) => {
            // Fallback to raw SCSI if libfreemkv can't match the drive
            eprintln!("{}", e);
            std::process::exit(1);
        }
    };

    let mut dev = DiscReader(&mut session);

    // Read capacity
    let capacity = read_capacity(&mut dev);

    // Read UDF structure
    let udf = match parse_udf(&mut dev) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("{}: {}", strings::get("error.not_bluray"), e);
            std::process::exit(1);
        }
    };

    // Read disc title from META/DL/bdmt_eng.xml
    let disc_title = read_disc_title(&mut dev, &udf);

    // Try to extract track labels from BD-J JAR files (skip with --basic)
    let jar_labels = if basic { Vec::new() } else { read_jar_labels(&mut dev, &udf) };
    let mut audio_jar: Vec<libfreemkv::jar::TrackLabel> = jar_labels.iter()
        .filter(|l| !l.hint.starts_with("PGStream")).cloned().collect();
    let sub_jar: Vec<&libfreemkv::jar::TrackLabel> = jar_labels.iter()
        .filter(|l| l.hint.starts_with("PGStream")).collect();

    // Find and parse all playlists
    let mut titles = Vec::new();
    for mpls_entry in &udf.mpls_files {
        let mpls_data = match udf.read_file(&mut dev, &mpls_entry.lba, mpls_entry.size) {
            Ok(d) => d,
            Err(_) => continue,
        };
        if let Some(title) = parse_mpls_title(&mpls_entry.name, &mpls_data, &mut dev, &udf) {
            titles.push(title);
        }
    }

    // Sort by duration (longest first), then playlist name as tiebreaker (stable, deterministic)
    titles.sort_by(|a, b| {
        b.duration_secs.partial_cmp(&a.duration_secs)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.playlist_name.cmp(&b.playlist_name))
    });

    // Display
    if !quiet {
        if let Some(ref title) = disc_title {
            println!("Disc: {}", title);
        }
        if let Some(cap) = capacity {
            let gb = cap as f64 * 2048.0 / (1024.0 * 1024.0 * 1024.0);
            println!("{}: {} sectors ({:.1} GB)", strings::get("disc.capacity"), fmt_num(cap as u64), gb);
        }
        println!();
    }

    if titles.is_empty() {
        println!("{}", strings::get("disc.no_titles"));
        return;
    }

    println!("{}", strings::get("disc.titles"));
    println!();


    let max_titles = if full { titles.len() } else { 5.min(titles.len()) };

    for (i, title) in titles.iter().enumerate().take(max_titles) {

        let hrs = (title.duration_secs / 3600.0) as u32;
        let mins = ((title.duration_secs % 3600.0) / 60.0) as u32;
        let size_gb = title.size_bytes as f64 / (1024.0 * 1024.0 * 1024.0);


        let clip_word = if title.clip_count == 1 {
            strings::get("disc.clip")
        } else {
            strings::get("disc.clips")
        };

        println!("  {:2}. {:14}  {:1}h {:02}m  {:>5.1} GB  {} {}",
            i + 1,
            title.playlist_name,
            hrs, mins,
            size_gb,
            title.clip_count,
            clip_word,
        );

        // Show streams for all visible titles
        if !quiet && !title.streams.is_empty() {
            // Build stream descriptions: JAR mode (enhanced) or basic (disc only)
            let has_jar = !basic && (!audio_jar.is_empty() || !sub_jar.is_empty());

            // Reset audio JAR labels for each title (clone from original)
            let mut title_audio_jar: Vec<libfreemkv::jar::TrackLabel> = if has_jar {
                jar_labels.iter()
                    .filter(|l| !l.hint.starts_with("PGStream"))
                    .cloned().collect()
            } else {
                Vec::new()
            };

            println!();

            // Group streams by kind
            let videos: Vec<&StreamInfo> = title.streams.iter().filter(|s| s.kind == StreamKind::Video).collect();
            let audios: Vec<&StreamInfo> = title.streams.iter().filter(|s| s.kind == StreamKind::Audio).collect();
            let subs: Vec<&StreamInfo> = title.streams.iter().filter(|s| s.kind == StreamKind::Subtitle).collect();

            // Reset JAR audio labels for this title
            let mut title_audio_jar: Vec<libfreemkv::jar::TrackLabel> = if has_jar {
                jar_labels.iter().filter(|l| !l.hint.starts_with("PGStream")).cloned().collect()
            } else { Vec::new() };

            let indent = "                 ";

            // Video
            if !videos.is_empty() {
                for (vi, v) in videos.iter().enumerate() {
                    let line = if has_jar && v.description.contains("[Dolby Vision EL]") {
                        format_video_jar(v, true)
                    } else {
                        format_video_jar(v, false)
                    };
                    if vi == 0 {
                        println!("      Video:     {}", line);
                    } else {
                        println!("{}{}", indent, line);
                    }
                }
                println!();
            }

            // Audio
            if !audios.is_empty() {
                for (ai, a) in audios.iter().enumerate() {
                    let line = if has_jar {
                        format_audio_jar(a, &mut title_audio_jar)
                    } else {
                        format_audio_basic(a)
                    };
                    if ai == 0 {
                        println!("      Audio:     {}", line);
                    } else {
                        println!("{}{}", indent, line);
                    }
                }
                println!();
            }

            // Subtitle
            if !subs.is_empty() {
                for (si, s) in subs.iter().enumerate() {
                    let line = if has_jar {
                        format_sub_jar(s, si, sub_jar.len())
                    } else {
                        format_sub_basic(s)
                    };
                    if si == 0 {
                        println!("      Subtitle:  {}", line);
                    } else {
                        println!("{}{}", indent, line);
                    }
                }
                println!();
            }
        }

    }

    if !full && titles.len() > max_titles {
        println!("      +{} more (use --full to show all)", titles.len() - max_titles);
    }
}

// ── UDF structures ──────────────────────────────────────────────────────────

struct UdfInfo {
    partition_start: u32,
    metadata_start: u32,
    mpls_files: Vec<FileRef>,
    clpi_files: Vec<FileRef>,
    jar_files: Vec<FileRef>,
    meta_files: Vec<FileRef>,
}

struct FileRef {
    name: String,
    lba: u32,  // metadata-relative
    size: u32,
}

impl UdfInfo {
    fn read_file(&self, dev: &mut DiscReader, meta_lba: &u32, size: u32) -> Result<Vec<u8>, String> {
        // Read ICB from metadata partition
        let icb = read_sector_raw(dev, self.metadata_start + meta_lba)?;
        let tag = u16::from_le_bytes([icb[0], icb[1]]);

        let ad_off = match tag {
            266 => {
                let l_ea = u32::from_le_bytes([icb[208], icb[209], icb[210], icb[211]]) as usize;
                216 + l_ea
            }
            261 => {
                let l_ea = u32::from_le_bytes([icb[168], icb[169], icb[170], icb[171]]) as usize;
                176 + l_ea
            }
            _ => return Err(format!("bad ICB tag {}", tag)),
        };

        let data_len = u32::from_le_bytes([icb[ad_off], icb[ad_off+1], icb[ad_off+2], icb[ad_off+3]]) & 0x3FFFFFFF;
        let data_lba = u32::from_le_bytes([icb[ad_off+4], icb[ad_off+5], icb[ad_off+6], icb[ad_off+7]]);
        // File data lives on the PHYSICAL partition, not the metadata partition
        let abs_start = self.partition_start + data_lba;

        let sector_count = (data_len + 2047) / 2048;
        let mut data = vec![0u8; sector_count as usize * 2048];
        for i in 0..sector_count {
            let sec = read_sector_raw(dev, abs_start + i)?;
            data[i as usize * 2048..(i as usize + 1) * 2048].copy_from_slice(&sec);
        }
        data.truncate(size.min(data_len) as usize);
        Ok(data)
    }

    fn find_clpi(&self, clip_id: &str) -> Option<&FileRef> {
        let name = format!("{}.clpi", clip_id);
        self.clpi_files.iter().find(|f| f.name.eq_ignore_ascii_case(&name))
    }
}

// ── UDF parser ──────────────────────────────────────────────────────────────

fn parse_udf(dev: &mut DiscReader) -> Result<UdfInfo, String> {
    // AVDP at sector 256
    let avdp = read_sector_raw(dev, 256)?;
    if u16::from_le_bytes([avdp[0], avdp[1]]) != 2 {
        return Err("no UDF AVDP at sector 256".into());
    }

    // VDS — find partition descriptor + logical volume descriptor
    let mut partition_start: u32 = 0;
    let mut num_part_maps: u32 = 0;
    let mut lvd_sector: Option<u32> = None;

    for i in 32..64 {
        let desc = read_sector_raw(dev, i)?;
        let tag = u16::from_le_bytes([desc[0], desc[1]]);
        match tag {
            5 => partition_start = u32::from_le_bytes([desc[188], desc[189], desc[190], desc[191]]),
            6 => { num_part_maps = u32::from_le_bytes([desc[268], desc[269], desc[270], desc[271]]); lvd_sector = Some(i); }
            8 => break,
            _ => {}
        }
    }

    if partition_start == 0 {
        return Err("no partition descriptor".into());
    }

    // Metadata partition (UDF 2.50)
    let metadata_start = if num_part_maps >= 2 {
        let lvd = read_sector_raw(dev, lvd_sector.unwrap())?;
        let pm1_len = lvd[441] as usize;
        let pm2_type = if pm1_len > 0 && 440 + pm1_len < 2048 { lvd[440 + pm1_len] } else { 0 };

        if pm2_type == 2 {
            let meta_icb = read_sector_raw(dev, partition_start)?;
            if u16::from_le_bytes([meta_icb[0], meta_icb[1]]) == 266 {
                let l_ea = u32::from_le_bytes([meta_icb[208], meta_icb[209], meta_icb[210], meta_icb[211]]) as usize;
                let ad_off = 216 + l_ea;
                let ad_pos = u32::from_le_bytes([meta_icb[ad_off+4], meta_icb[ad_off+5], meta_icb[ad_off+6], meta_icb[ad_off+7]]);
                partition_start + ad_pos
            } else { partition_start }
        } else { partition_start }
    } else { partition_start };

    // FSD
    let fsd = read_sector_raw(dev, metadata_start)?;
    if u16::from_le_bytes([fsd[0], fsd[1]]) != 256 {
        return Err("no FSD".into());
    }
    let root_lba = u32::from_le_bytes([fsd[404], fsd[405], fsd[406], fsd[407]]);

    // Walk directories to find BDMV/PLAYLIST and BDMV/CLIPINF
    let mut mpls_files = Vec::new();
    let mut clpi_files = Vec::new();
    let mut jar_files = Vec::new();
    let mut meta_files = Vec::new();

    // Read root dir
    let root_entries = read_dir_entries(dev, metadata_start, root_lba)?;

    // Find BDMV
    if let Some(bdmv) = root_entries.iter().find(|e| e.name.eq_ignore_ascii_case("BDMV") && e.is_dir) {
        let bdmv_entries = read_dir_entries(dev, metadata_start, bdmv.lba)?;

        // PLAYLIST
        if let Some(pl_dir) = bdmv_entries.iter().find(|e| e.name.eq_ignore_ascii_case("PLAYLIST") && e.is_dir) {
            let pl_entries = read_dir_entries(dev, metadata_start, pl_dir.lba)?;
            for e in pl_entries {
                if e.name.ends_with(".mpls") {
                    mpls_files.push(FileRef { name: e.name, lba: e.lba, size: e.size });
                }
            }
        }

        // CLIPINF
        if let Some(cl_dir) = bdmv_entries.iter().find(|e| e.name.eq_ignore_ascii_case("CLIPINF") && e.is_dir) {
            let cl_entries = read_dir_entries(dev, metadata_start, cl_dir.lba)?;
            for e in cl_entries {
                if e.name.ends_with(".clpi") {
                    clpi_files.push(FileRef { name: e.name, lba: e.lba, size: e.size });
                }
            }
        }

        // JAR (BD-J menu applications — contain track labels)
        if let Some(jar_dir) = bdmv_entries.iter().find(|e| e.name.eq_ignore_ascii_case("JAR") && e.is_dir) {
            let jar_entries = read_dir_entries(dev, metadata_start, jar_dir.lba)?;
            for e in jar_entries {
                if e.name.ends_with(".jar") {
                    jar_files.push(FileRef { name: e.name, lba: e.lba, size: e.size });
                }
            }
        }

        // META/DL (disc title, thumbnails)
        if let Some(meta_dir) = bdmv_entries.iter().find(|e| e.name.eq_ignore_ascii_case("META") && e.is_dir) {
            let meta_entries = read_dir_entries(dev, metadata_start, meta_dir.lba)?;
            if let Some(dl_dir) = meta_entries.iter().find(|e| e.name.eq_ignore_ascii_case("DL") && e.is_dir) {
                let dl_entries = read_dir_entries(dev, metadata_start, dl_dir.lba)?;
                for e in dl_entries {
                    if e.name.ends_with(".xml") {
                        meta_files.push(FileRef { name: e.name, lba: e.lba, size: e.size });
                    }
                }
            }
        }
    }

    Ok(UdfInfo { partition_start, metadata_start, mpls_files, clpi_files, jar_files, meta_files })
}

struct DirEntryInfo {
    name: String,
    is_dir: bool,
    lba: u32,
    size: u32,
}

fn read_dir_entries(dev: &mut DiscReader, meta_start: u32, dir_meta_lba: u32) -> Result<Vec<DirEntryInfo>, String> {
    let icb = read_sector_raw(dev, meta_start + dir_meta_lba)?;
    let tag = u16::from_le_bytes([icb[0], icb[1]]);

    let ad_off = match tag {
        266 => { let l_ea = u32::from_le_bytes([icb[208], icb[209], icb[210], icb[211]]) as usize; 216 + l_ea }
        261 => { let l_ea = u32::from_le_bytes([icb[168], icb[169], icb[170], icb[171]]) as usize; 176 + l_ea }
        _ => return Ok(Vec::new()),
    };

    let ad_len = u32::from_le_bytes([icb[ad_off], icb[ad_off+1], icb[ad_off+2], icb[ad_off+3]]) & 0x3FFFFFFF;
    let ad_pos = u32::from_le_bytes([icb[ad_off+4], icb[ad_off+5], icb[ad_off+6], icb[ad_off+7]]);

    let sector_count = ((ad_len + 2047) / 2048).min(64);
    let mut dir_data = Vec::new();
    for i in 0..sector_count {
        let sec = read_sector_raw(dev, meta_start + ad_pos + i)?;
        dir_data.extend_from_slice(&sec);
    }

    let mut entries = Vec::new();
    let mut pos = 0;

    while pos + 38 < dir_data.len().min(ad_len as usize) {
        if u16::from_le_bytes([dir_data[pos], dir_data[pos+1]]) != 257 { break; }

        let file_chars = dir_data[pos + 18];
        let l_fi = dir_data[pos + 19] as usize;
        let icb_lba = u32::from_le_bytes([dir_data[pos+24], dir_data[pos+25], dir_data[pos+26], dir_data[pos+27]]);
        let l_iu = u16::from_le_bytes([dir_data[pos+36], dir_data[pos+37]]) as usize;

        let is_dir = (file_chars & 0x02) != 0;
        let is_parent = (file_chars & 0x08) != 0;

        if !is_parent && l_fi > 0 {
            let raw = &dir_data[pos+38+l_iu..pos+38+l_iu+l_fi];
            let name = parse_udf_name(raw);
            if !name.is_empty() {
                let size = read_file_size(dev, meta_start, icb_lba).unwrap_or(0);
                entries.push(DirEntryInfo { name, is_dir, lba: icb_lba, size });
            }
        }

        pos += ((38 + l_iu + l_fi + 3) & !3) as usize;
    }

    Ok(entries)
}

fn read_file_size(dev: &mut DiscReader, meta_start: u32, meta_lba: u32) -> Result<u32, String> {
    let icb = read_sector_raw(dev, meta_start + meta_lba)?;
    let tag = u16::from_le_bytes([icb[0], icb[1]]);
    match tag {
        261 | 266 => Ok(u32::from_le_bytes([icb[56], icb[57], icb[58], icb[59]])),
        _ => Ok(0),
    }
}

fn parse_udf_name(data: &[u8]) -> String {
    if data.is_empty() { return String::new(); }
    match data[0] {
        8 => String::from_utf8_lossy(&data[1..]).trim().to_string(),
        16 => {
            let mut s = String::new();
            for i in (1..data.len()).step_by(2) {
                if i + 1 < data.len() {
                    if let Some(ch) = char::from_u32(((data[i] as u32) << 8) | data[i+1] as u32) {
                        s.push(ch);
                    }
                }
            }
            s.trim().to_string()
        }
        _ => String::from_utf8_lossy(&data[1..]).trim().to_string(),
    }
}

// ── MPLS parser ─────────────────────────────────────────────────────────────

struct TitleInfo {
    playlist_name: String,
    duration_secs: f64,
    size_bytes: u64,
    clip_count: usize,
    streams: Vec<StreamInfo>,
}

#[derive(Clone, Copy, PartialEq)]
enum StreamKind { Video, Audio, Subtitle }

#[derive(Clone)]
struct StreamInfo {
    kind: StreamKind,
    pid: u16,
    description: String,
}

fn parse_mpls_title(name: &str, data: &[u8], dev: &mut DiscReader, udf: &UdfInfo) -> Option<TitleInfo> {
    if data.len() < 40 || &data[0..4] != b"MPLS" { return None; }

    let pl_start = u32::from_be_bytes([data[8], data[9], data[10], data[11]]) as usize;
    if pl_start + 10 > data.len() { return None; }

    let pl = &data[pl_start..];
    let num_items = u16::from_be_bytes([pl[6], pl[7]]) as usize;

    let mut total_ticks: u64 = 0;
    let mut total_size: u64 = 0;
    let mut clip_count = 0;
    let mut streams = Vec::new();
    let mut pos = 10;

    for item_idx in 0..num_items {
        if pl_start + pos + 2 > data.len() { break; }
        let item_len = u16::from_be_bytes([pl[pos], pl[pos+1]]) as usize;
        if pos + 2 + item_len > pl.len() { break; }

        let item = &pl[pos+2..pos+2+item_len];
        if item.len() < 20 { pos += 2 + item_len; continue; }

        let clip_id = String::from_utf8_lossy(&item[0..5]).to_string();
        let in_time = u32::from_be_bytes([item[12], item[13], item[14], item[15]]);
        let out_time = u32::from_be_bytes([item[16], item[17], item[18], item[19]]);

        total_ticks += (out_time as u64).saturating_sub(in_time as u64);
        clip_count += 1;

        // Estimate size from CLPI source packet count
        if let Some(clpi_ref) = udf.find_clpi(&clip_id) {
            if let Ok(clpi_data) = udf.read_file(dev, &clpi_ref.lba, clpi_ref.size) {
                if clpi_data.len() > 60 {
                    let pkt_count = u32::from_be_bytes([clpi_data[56], clpi_data[57], clpi_data[58], clpi_data[59]]);
                    total_size += pkt_count as u64 * 192;
                }
            }
        }

        // Parse streams from STN table (first play item only)
        // PlayItem layout:
        //   [0:5]   clip_id (5)
        //   [5:9]   codec_id (4)
        //   [9:11]  flags (2: reserved + is_multi_angle + connection_condition)
        //   [11]    stc_id (1)
        //   [12:16] in_time (4)
        //   [16:20] out_time (4)
        //   [20:28] UO mask (8 bytes = 64 bits)
        //   [28]    flags (1: random_access etc)
        //   [29]    still_mode (1)
        //   [30:32] still_time (2)
        //   [32:]   STN table
        const STN_OFFSET: usize = 32;
        if item_idx == 0 && item.len() > STN_OFFSET + 16 {
            let stn_off = STN_OFFSET;
            if stn_off + 16 < item.len() {
                let stn_len = u16::from_be_bytes([item[stn_off], item[stn_off+1]]) as usize;
                if stn_len > 14 {
                    // STN header: length(2) + reserved(2) + counts(8) + reserved(4) = 16 bytes
                    //   [+4] primary_video  [+5] primary_audio  [+6] pg_subtitle
                    //   [+7] ig_interactive  [+8] secondary_audio  [+9] secondary_video
                    //   [+10] pip_pg  [+11] dolby_vision_el
                    let n_video = item[stn_off + 4] as usize;
                    let n_audio = item[stn_off + 5] as usize;
                    let n_pg = item[stn_off + 6] as usize;
                    let n_ig = item[stn_off + 7] as usize;
                    let n_sec_audio = item[stn_off + 8] as usize;
                    let n_sec_video = item[stn_off + 9] as usize;
                    let _n_pip_pg = item[stn_off + 10] as usize;
                    let n_dv = item[stn_off + 11] as usize;

                    // Stream entries start after 16-byte STN header
                    let mut spos = stn_off + 16;

                    // Primary video
                    for _ in 0..n_video {
                        if let Some((s, next)) = parse_stream_entry(item, spos, StreamKind::Video) {
                            streams.push(s);
                            spos = next;
                        } else { break; }
                    }
                    // Primary audio
                    for _ in 0..n_audio {
                        if let Some((s, next)) = parse_stream_entry(item, spos, StreamKind::Audio) {
                            streams.push(s);
                            spos = next;
                        } else { break; }
                    }
                    // PG subtitles
                    for _ in 0..n_pg {
                        if let Some((s, next)) = parse_stream_entry(item, spos, StreamKind::Subtitle) {
                            streams.push(s);
                            spos = next;
                        } else { break; }
                    }
                    // IG (interactive graphics / menus) — skip but advance position
                    for _ in 0..n_ig {
                        if let Some((_, next)) = parse_stream_entry(item, spos, StreamKind::Subtitle) {
                            spos = next;
                        } else { break; }
                    }
                    // Secondary audio (e.g. commentary with Atmos)
                    for _ in 0..n_sec_audio {
                        if let Some((mut s, next)) = parse_stream_entry(item, spos, StreamKind::Audio) {
                            s.description = format!("{} [secondary]", s.description);
                            streams.push(s);
                            // Skip the extra ref bytes after secondary audio entries
                            // num_primary_audio_ref(1) + reserved(1) + refs(N) + padding
                            let refs_start = next;
                            if refs_start < item.len() {
                                let n_refs = item[refs_start] as usize;
                                let padded = 2 + n_refs + (n_refs % 2);
                                spos = refs_start + padded;
                            } else { spos = next; }
                        } else { break; }
                    }
                    // Secondary video (PiP)
                    for _ in 0..n_sec_video {
                        if let Some((mut s, next)) = parse_stream_entry(item, spos, StreamKind::Video) {
                            s.description = format!("{} [secondary]", s.description);
                            streams.push(s);
                            // Skip extra ref bytes
                            let refs_start = next;
                            if refs_start + 2 < item.len() {
                                let n_arefs = item[refs_start] as usize;
                                let after_arefs = refs_start + 2 + n_arefs + (n_arefs % 2);
                                if after_arefs < item.len() {
                                    let n_prefs = item[after_arefs] as usize;
                                    spos = after_arefs + 2 + n_prefs + (n_prefs % 2);
                                } else { spos = after_arefs; }
                            } else { spos = next; }
                        } else { break; }
                    }
                    // Dolby Vision enhancement layer
                    for _ in 0..n_dv {
                        if let Some((mut s, next)) = parse_stream_entry(item, spos, StreamKind::Video) {
                            s.description = format!("{} [Dolby Vision EL]", s.description);
                            streams.push(s);
                            spos = next;
                        } else { break; }
                    }
                }
            }
        }

        pos += 2 + item_len;
    }

    let duration_secs = total_ticks as f64 / 45000.0;
    if duration_secs < 1.0 { return None; }

    Some(TitleInfo {
        playlist_name: name.to_string(),
        duration_secs,
        size_bytes: total_size,
        clip_count,
        streams,
    })
}

fn parse_stream_entry(item: &[u8], pos: usize, kind: StreamKind) -> Option<(StreamInfo, usize)> {
    if pos + 2 > item.len() { return None; }

    // StreamEntry: length(1) + data
    let se_len = item[pos] as usize;
    let se_end = pos + 1 + se_len;
    if se_end > item.len() { return None; }

    // StreamAttributes: length(1) + coding_type(1) + ...
    if se_end + 2 > item.len() { return None; }
    let sa_len = item[se_end] as usize;
    let sa_end = se_end + 1 + sa_len;
    if sa_end > item.len() { return None; }

    let coding_type = item[se_end + 1];
    let sa = &item[se_end + 1..se_end + 1 + sa_len];

    let desc = match kind {
        StreamKind::Video => {
            let format = if sa.len() > 1 { sa[1] >> 4 } else { 0 };
            let rate = if sa.len() > 1 { sa[1] & 0x0F } else { 0 };
            let res = match format { 4 => "1080i", 5 => "720p", 6 => "1080p", 8 => "2160p", _ => "?" };
            let fps = match rate { 1 => "23.976", 2 => "24", 3 => "25", 4 => "29.97", 6 => "50", 7 => "59.94", _ => "?" };
            let codec = match coding_type { 0x1B => "H.264", 0x24 => "HEVC", 0xEA => "VC-1", 0x02 => "MPEG-2", _ => "?" };

            let mut extra = String::new();
            if coding_type == 0x24 && sa.len() > 2 {
                let dr = (sa[2] >> 4) & 0x0F;
                let cs = (sa[2]) & 0x0F;
                match dr { 1 => extra.push_str(" HDR10"), 2 => extra.push_str(" Dolby Vision"), _ => {} }
                match cs { 2 => extra.push_str(" BT.2020"), _ => {} }
            }
            format!("{} {} {}fps{}", codec, res, fps, extra)
        }
        StreamKind::Audio => {
            let fmt = if sa.len() > 1 { sa[1] >> 4 } else { 0 };
            let rate = if sa.len() > 1 { sa[1] & 0x0F } else { 0 };
            let lang = if sa.len() > 4 { String::from_utf8_lossy(&sa[2..5]).to_string() } else { "???".into() };
            let ch = match fmt { 1 => "mono", 3 => "stereo", 6 => "5.1", 12 => "7.1", _ => "?" };
            let sr = match rate { 1 => "48kHz", 4 => "96kHz", 5 => "192kHz", _ => "?" };
            let codec = match coding_type {
                0x80 => "LPCM", 0x81 => "AC-3", 0x82 => "DTS", 0x83 => "TrueHD",
                0x84 => "AC-3+", 0x85 => "DTS-HD HR", 0x86 => "DTS-HD MA",
                0xA1 => "AC-3+", 0xA2 => "DTS-HD", _ => "?"
            };
            format!("{} {} {} ({})", codec, ch, sr, lang)
        }
        StreamKind::Subtitle => {
            let lang = if sa.len() > 3 { String::from_utf8_lossy(&sa[1..4]).to_string() } else { "???".into() };
            format!("PGS ({})", lang)
        }
    };

    // Extract PID from stream entry (type 0x01: PID at bytes 2-3)
    let pid = if item[pos + 1] == 0x01 && pos + 4 <= item.len() {
        u16::from_be_bytes([item[pos + 2], item[pos + 3]])
    } else {
        0
    };

    Some((StreamInfo { kind, pid, description: desc }, sa_end))
}

// ── Sector I/O ──────────────────────────────────────────────────────────────

fn read_sector_raw(dev: &mut DiscReader, lba: u32) -> Result<Vec<u8>, String> {
    dev.read_sector(lba)
}

fn read_capacity(dev: &mut DiscReader) -> Option<u32> {
    dev.read_capacity()
}

/// Format a number with comma separators: 41288703 → "41,288,703"
fn fmt_num(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 { result.push(','); }
        result.push(c);
    }
    result.chars().rev().collect()
}

fn find_bd_drive() -> Option<String> {
    for i in 0..16 {
        let path = format!("/dev/sg{}", i);
        if std::path::Path::new(&path).exists() {
            // Try to open as a drive — if it works, it's our device
            if libfreemkv::DriveSession::open(std::path::Path::new(&path)).is_ok() {
                return Some(path);
            }
        }
    }
    None
}

// ── JAR label extraction ────────────────────────────────────────────────────

// ── Stream formatting ───────────────────────────────────────────────────────

/// Language code → full name
fn lang_name(code: &str) -> &str {
    match code {
        "eng" => "English", "fra" | "fre" => "French", "spa" => "Spanish",
        "deu" | "ger" => "German", "ita" => "Italian", "por" => "Portuguese",
        "jpn" => "Japanese", "zho" | "chi" => "Chinese", "kor" => "Korean",
        "rus" => "Russian", "ara" => "Arabic", "hin" => "Hindi",
        "dan" => "Danish", "fin" => "Finnish", "nor" => "Norwegian", "swe" => "Swedish",
        "nld" | "dut" => "Dutch", "pol" => "Polish", "tur" => "Turkish",
        "tha" => "Thai", "ces" | "cze" => "Czech", "hun" => "Hungarian",
        "ron" | "rum" => "Romanian", "hrv" => "Croatian", "slk" | "slo" => "Slovak",
        "bul" => "Bulgarian", "ukr" => "Ukrainian", "heb" => "Hebrew",
        "ell" | "gre" => "Greek", "cat" => "Catalan", "ind" => "Indonesian",
        "msa" | "may" => "Malay", "vie" => "Vietnamese",
        _ => code,
    }
}

/// Codec display name (menu style)
fn codec_display(desc: &str) -> &str {
    if desc.contains("TrueHD") { "TrueHD" }
    else if desc.contains("DTS-HD MA") { "DTS-HD MA" }
    else if desc.contains("DTS-HD HR") { "DTS-HD HR" }
    else if desc.contains("DTS") { "DTS" }
    else if desc.contains("AC-3+") { "DD+" }
    else if desc.contains("AC-3") { "DD" }
    else if desc.contains("LPCM") { "LPCM" }
    else if desc.contains("HEVC") { "HEVC" }
    else if desc.contains("H.264") { "H.264" }
    else if desc.contains("VC-1") { "VC-1" }
    else if desc.contains("MPEG-2") { "MPEG-2" }
    else { "?" }
}

/// Extract channels from description (e.g. "5.1", "7.1", "stereo")
fn extract_channels(desc: &str) -> &str {
    if desc.contains("7.1") { "7.1" }
    else if desc.contains("5.1") { "5.1" }
    else if desc.contains("stereo") { "stereo" }
    else if desc.contains("mono") { "mono" }
    else { "" }
}

fn format_video_jar(v: &StreamInfo, is_dv_el: bool) -> String {
    if is_dv_el {
        // DV enhancement layer
        let res = if v.description.contains("2160p") { "2160p" }
                  else if v.description.contains("1080p") { "1080p" }
                  else { "" };
        format!("Dolby Vision EL {}", res)
    } else {
        // Primary video — keep technical details
        let codec = codec_display(&v.description);
        let mut parts = vec![codec.to_string()];
        if v.description.contains("2160p") { parts.push("2160p".into()); }
        else if v.description.contains("1080p") { parts.push("1080p".into()); }
        else if v.description.contains("1080i") { parts.push("1080i".into()); }
        else if v.description.contains("720p") { parts.push("720p".into()); }
        if v.description.contains("HDR10") { parts.push("HDR10".into()); }
        if v.description.contains("Dolby Vision") { parts.push("Dolby Vision".into()); }
        if v.description.contains("BT.2020") { parts.push("BT.2020".into()); }
        parts.join(" ")
    }
}

fn format_audio_jar(a: &StreamInfo, jar: &mut Vec<libfreemkv::jar::TrackLabel>) -> String {
    let lang = extract_lang(&a.description);
    let full_lang = lang_name(&lang);
    let codec = codec_display(&a.description);
    let ch = extract_channels(&a.description);

    // Match JAR label
    let is_truehd = a.description.contains("TrueHD");
    let idx = if is_truehd {
        jar.iter().position(|l| l.language == lang && l.hint == "MLP")
    } else {
        jar.iter().position(|l| l.language == lang && l.hint == "AC3")
            .or_else(|| jar.iter().position(|l| l.language == lang && l.hint == "ADES"))
            .or_else(|| jar.iter().position(|l| l.language == lang && l.hint.starts_with("AudioStream")))
    };

    let extra = if let Some(i) = idx {
        let label = jar.remove(i);
        if label.description.is_empty() { String::new() }
        else { format!(" ({})", label.description) }
    } else { String::new() };

    if ch.is_empty() {
        format!("{} {}{}", full_lang, codec, extra)
    } else {
        format!("{} {} {}{}", full_lang, codec, ch, extra)
    }
}

fn format_audio_basic(a: &StreamInfo) -> String {
    let lang = extract_lang(&a.description);
    let full_lang = lang_name(&lang);
    let codec = codec_display(&a.description);
    let ch = extract_channels(&a.description);

    if ch.is_empty() {
        format!("{} {}", full_lang, codec)
    } else {
        format!("{} {} {}", full_lang, codec, ch)
    }
}

fn format_sub_jar(s: &StreamInfo, idx: usize, labeled_count: usize) -> String {
    let lang = extract_lang(&s.description);
    let full_lang = lang_name(&lang);
    if labeled_count > 0 && idx >= labeled_count {
        format!("{} (forced)", full_lang)
    } else {
        full_lang.to_string()
    }
}

fn format_sub_basic(s: &StreamInfo) -> String {
    let lang = extract_lang(&s.description);
    lang_name(&lang).to_string()
}

fn read_disc_title(dev: &mut DiscReader, udf: &UdfInfo) -> Option<String> {
    for entry in &udf.meta_files {
        let data = udf.read_file(dev, &entry.lba, entry.size).ok()?;
        let xml = String::from_utf8_lossy(&data);
        // Extract <di:name>...</di:name>
        if let Some(start) = xml.find("<di:name>") {
            let start = start + "<di:name>".len();
            if let Some(end) = xml[start..].find("</di:name>") {
                let title = xml[start..start + end].trim().to_string();
                if !title.is_empty() {
                    return Some(title);
                }
            }
        }
    }
    None
}

fn read_jar_labels(dev: &mut DiscReader, udf: &UdfInfo) -> Vec<libfreemkv::jar::TrackLabel> {
    for entry in &udf.jar_files {
        let jar_data = match udf.read_file(dev, &entry.lba, entry.size) {
            Ok(d) => d,
            Err(_) => continue,
        };

        if let Some(labels) = libfreemkv::jar::extract_labels(&jar_data) {
            let mut all = labels.audio;
            all.extend(labels.subtitle);
            return all;
        }
    }
    Vec::new()
}

fn find_jar_audio_label(labels: &[libfreemkv::jar::TrackLabel], stream: &StreamInfo) -> Option<String> {
    // Find audio labels only
    let audio_labels: Vec<&libfreemkv::jar::TrackLabel> = labels.iter()
        .filter(|l| !l.hint.starts_with("PGStream"))
        .collect();

    if audio_labels.is_empty() { return None; }

    // Match by language + codec type
    // For each stream, find the corresponding label
    // Strategy: filter labels by language, then match by codec hint
    let lang = extract_lang(&stream.description);
    let is_truehd = stream.description.contains("TrueHD");
    let is_ac3 = stream.description.contains("AC-3");

    let matching: Vec<&&libfreemkv::jar::TrackLabel> = audio_labels.iter()
        .filter(|l| l.language == lang)
        .collect();

    if matching.is_empty() { return None; }

    // If TrueHD, find MLP label
    if is_truehd {
        if let Some(l) = matching.iter().find(|l| l.hint == "MLP") {
            if !l.description.is_empty() { return Some(l.description.clone()); }
        }
    }

    // If AC-3, match by position among same-lang AC-3 labels
    if is_ac3 {
        let ac3_labels: Vec<&&libfreemkv::jar::TrackLabel> = matching.iter()
            .filter(|l| l.hint == "AC3" || l.hint == "ADES" || l.hint.starts_with("AudioStream"))
            .cloned()
            .collect();

        // We need to know which AC-3 stream this is (1st, 2nd, 3rd for this language)
        // Use the PID to determine position — but we don't have all PIDs here
        // For now, return the ADES labels if they exist (those are the important ones)
        for l in &ac3_labels {
            if l.hint == "ADES" && !l.description.is_empty() {
                // Check if this stream's PID matches the ADES position
                // We can't perfectly match without tracking position, but if there are
                // ADES labels, show them for the higher-PID AC-3 streams
                return Some(l.description.clone());
            }
        }

        // First AC-3 of this language = compatibility track
        if let Some(l) = ac3_labels.first() {
            if l.hint == "AC3" && !l.description.is_empty() {
                return Some(l.description.clone());
            }
        }
    }

    None
}

fn find_jar_subtitle_label(labels: &[libfreemkv::jar::TrackLabel], stream: &StreamInfo) -> Option<String> {
    // Subtitle labels are simple — just lang_PGStream{n}
    // The duplicate fra/spa at the end with no labels are likely forced
    let sub_labels: Vec<&libfreemkv::jar::TrackLabel> = labels.iter()
        .filter(|l| l.hint.starts_with("PGStream"))
        .collect();

    if sub_labels.is_empty() { return None; }

    let lang = extract_lang(&stream.description);

    // Check if this language has a PGStream label
    let has_label = sub_labels.iter().any(|l| l.language == lang);

    if !has_label && !lang.is_empty() {
        // Language appears in stream but NOT in JAR labels
        // If there's another stream with this language that IS labeled,
        // this one is likely a forced subtitle
        let labeled_langs: Vec<&str> = sub_labels.iter().map(|l| l.language.as_str()).collect();
        if labeled_langs.contains(&lang.as_str()) {
            return Some("forced".to_string());
        }
    }

    None
}

fn extract_lang(description: &str) -> String {
    // Extract 3-letter language code from description like "AC-3 5.1 48kHz (eng)"
    if let Some(start) = description.rfind('(') {
        if let Some(end) = description.rfind(')') {
            let lang = &description[start+1..end];
            if lang.len() == 3 && lang.chars().all(|c| c.is_ascii_lowercase()) {
                return lang.to_string();
            }
        }
    }
    String::new()
}
