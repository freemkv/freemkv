# freemkv — Release Test Plan

Full end-to-end integration test suite for major releases.
Run against real hardware with real discs. Tests every stream
combination through the PES pipeline.

## Prerequisites

- BU40N drive (USB)
- KEYDB.cfg configured
- ~200 GB free disk space in /data/test/
- Network interface available for streaming tests
- Three discs: UHD, BD, DVD (swap when prompted)
- ffprobe and ffmpeg installed

## Discs Under Test

### UHD
- **Title:** DUNE (2021)
- **Format:** UHD Blu-ray (AACS 2.0)
- **Capacity:** 90.7 GB (47,533,152 sectors)
- **Video:** HEVC 2160p HDR10 / Dolby Vision
- **Audio:** TrueHD 7.1 Atmos, AC-3 5.1
- **Subtitles:** PGS (multiple languages)

### BD
- **Format:** Standard Blu-ray (AACS 1.0)
- **Video:** H.264 1080p
- **Audio:** DTS-HD MA 5.1, AC-3 5.1
- **Subtitles:** PGS

### DVD
- **Format:** DVD (CSS)
- **Video:** MPEG-2 480i/576i
- **Audio:** AC-3 5.1
- **Subtitles:** DVD VobSub

**Drive:** LG BU40N (USB)

## Notes

- Receiver starts BEFORE sender for network tests
- First-frame GOP errors (<10) are normal for BD HEVC content
- DV Enhancement Layer decode errors are expected and normal
- `--raw` means no decryption — encrypted bytes pass through
- Resume only applies to disc → ISO (raw sector copy)
- disc → ISO is a copy operation, not PES pipeline

---

## Phase 1: Error Handling (30 sec) — no disc needed

| # | Test | Command | Pass |
|---|------|---------|------|
| 1.1 | No scheme URL | `freemkv /dev/sr0 output.mkv` | Error message, exit 1 |
| 1.2 | Bad source URL | `freemkv foo://bar mkv://out.mkv` | Error, exit 1 |
| 1.3 | Missing ISO | `freemkv iso:///nonexistent.iso mkv://out.mkv` | File not found |
| 1.4 | No drive | `freemkv disc:///dev/sg99 mkv://out.mkv` | Device not found |
| 1.5 | Network unreachable | `freemkv network://192.0.2.1:9999 mkv://out.mkv` | Connection error |
| 1.6 | No args | `freemkv` | Usage, exit 0 |
| 1.7 | Help | `freemkv help` | Usage + examples |
| 1.8 | Version | `freemkv --version` | Version string |
| 1.9 | Invalid title | `freemkv iso://DUNE_UHD.iso mkv://out.mkv -t 999` | Title out of range |
| 1.10 | Null as input | `freemkv null:// mkv://out.mkv` | Write-only error |

## Phase 2: Disc Info [UHD] (1 min)

| # | Test | Command | Pass |
|---|------|---------|------|
| 2.1 | Disc info | `freemkv info disc://` | Titles, streams, durations |
| 2.2 | Disc info verbose | `freemkv info disc:// -v` | AACS/drive debug |
| 2.3 | Disc info device | `freemkv info disc:///dev/sg4` | Same as 2.1 |
| 2.4 | ISO info | `freemkv info iso://DUNE_UHD.iso` | Streams, 7 titles |
| 2.5 | MKV info | `freemkv info mkv://dune_t1.mkv` | Streams, duration |
| 2.6 | M2TS info | `freemkv info m2ts://dune_t1.m2ts` | Streams, duration |

## Phase 3: Disc → ISO (raw copy, not PES) [UHD] (~100 min)

| # | Test | Command | Pass |
|---|------|---------|------|
| 3.1 | Raw ISO rip | `freemkv disc:// iso://DUNE_UHD.iso --raw` | ~90.7 GB, no errors |
| 3.2 | Interrupt + resume | Start rip, Ctrl+C at ~5 GB, restart same command | Resumes from ~5 GB |
| 3.3 | Raw + verbose | `freemkv disc:// iso://raw_check.iso --raw -v` | Ctrl+C after 5s, file > 0 |

## Phase 4: ISO → All Outputs [UHD ISO] (~15 min)

Use a short title (-t 3, ~1:50) for quick tests. Title 1 for full verification.

| # | Test | Command | Pass |
|---|------|---------|------|
| 4.1 | ISO → MKV (short) | `freemkv iso://DUNE_UHD.iso mkv://t3.mkv -t 3` | ~196 MB MKV |
| 4.2 | ISO → MKV (main) | `freemkv iso://DUNE_UHD.iso mkv://t1.mkv -t 1` | ~8.8 GB MKV |
| 4.3 | ISO → M2TS | `freemkv iso://DUNE_UHD.iso m2ts://t3.m2ts -t 3` | ~622 MB M2TS |
| 4.4 | ISO → null | `freemkv iso://DUNE_UHD.iso null:// -t 3` | Exit 0, speed report |
| 4.5 | ISO → stdio | `freemkv iso://DUNE_UHD.iso stdio:// -t 3 -q > out.pes` | File created |
| 4.6 | ISO → network | receiver: `freemkv network://0.0.0.0:9000 mkv://net.mkv`<br>sender: `freemkv iso://DUNE_UHD.iso network://127.0.0.1:9000 -t 3` | MKV ~196 MB |
| 4.7 | ISO → MKV quiet | `freemkv iso://DUNE_UHD.iso mkv://quiet.mkv -t 3 -q` | No output, file OK |
| 4.8 | ISO → MKV verbose | `freemkv iso://DUNE_UHD.iso mkv://verbose.mkv -t 3 -v` | Debug info shown |
| 4.9 | ISO → network→M2TS | receiver: `freemkv network://0.0.0.0:9000 m2ts://net.m2ts`<br>sender: `freemkv iso://DUNE_UHD.iso network://127.0.0.1:9000 -t 3` | Valid M2TS |
| 4.10 | ISO → network→null | receiver: `freemkv network://0.0.0.0:9000 null://`<br>sender: `freemkv iso://DUNE_UHD.iso network://127.0.0.1:9000 -t 3` | Exit 0 both sides |

## Phase 5: Stream Verification [UHD ISO] (2 min)

Run on t1.mkv (main feature) from Phase 4.

| # | Check | Tool | Pass |
|---|-------|------|------|
| 5.1 | Video codec | `ffprobe t1.mkv` | HEVC 2160p |
| 5.2 | Primary audio | `ffprobe` | TrueHD 7.1 |
| 5.3 | Secondary audio | `ffprobe` | AC-3 5.1 |
| 5.4 | Subtitles | `ffprobe` | PGS tracks |
| 5.5 | HDR metadata | `ffprobe` | HDR10/DV flags |
| 5.6 | Playback test | `ffmpeg -v error -i t1.mkv -map 0:0 -t 30 -f null -` | <10 errors (DV EL normal) |
| 5.7 | File integrity | `ffprobe -v error t1.mkv` | exit 0 |

## Phase 6: M2TS as Input [UHD ISO] (5 min)

Uses M2TS from Phase 4.3.

| # | Test | Command | Pass |
|---|------|---------|------|
| 6.1 | M2TS → MKV | `freemkv m2ts://t3.m2ts mkv://m2ts_to_mkv.mkv` | ~196 MB, matches ISO→MKV |
| 6.2 | M2TS → M2TS | `freemkv m2ts://t3.m2ts m2ts://m2ts_remux.m2ts` | Similar size to original |
| 6.3 | M2TS → null | `freemkv m2ts://t3.m2ts null://` | Speed report |
| 6.4 | M2TS → network | receiver: `freemkv network://0.0.0.0:9000 mkv://m2ts_net.mkv`<br>sender: `freemkv m2ts://t3.m2ts network://127.0.0.1:9000` | Valid MKV |
| 6.5 | M2TS info | `freemkv info m2ts://t3.m2ts` | Streams, duration |
| 6.6 | M2TS → stdio | `freemkv m2ts://t3.m2ts stdio:// -q > m2ts_out.pes` | File created |

## Phase 7: MKV as Input [UHD ISO] (5 min)

Uses MKV from Phase 4.1.

| # | Test | Command | Pass |
|---|------|---------|------|
| 7.1 | MKV → M2TS | `freemkv mkv://t3.mkv m2ts://mkv_to_m2ts.m2ts` | Valid M2TS |
| 7.2 | MKV → MKV | `freemkv mkv://t3.mkv mkv://mkv_remux.mkv` | Same size as original |
| 7.3 | MKV → null | `freemkv mkv://t3.mkv null://` | Speed report |
| 7.4 | MKV info | `freemkv info mkv://t3.mkv` | Streams, duration |
| 7.5 | MKV → network | receiver: `freemkv network://0.0.0.0:9000 mkv://mkv_net.mkv`<br>sender: `freemkv mkv://t3.mkv network://127.0.0.1:9000` | Valid MKV |
| 7.6 | MKV → stdio | `freemkv mkv://t3.mkv stdio:// -q > mkv_out.pes` | File created |

## Phase 8: Stdio as Input [UHD ISO] (3 min)

Uses PES from Phase 4.5.

| # | Test | Command | Pass |
|---|------|---------|------|
| 8.1 | Stdio → MKV | `freemkv stdio:// mkv://stdio_to_mkv.mkv < out.pes` | Valid MKV |
| 8.2 | Stdio → M2TS | `freemkv stdio:// m2ts://stdio_to_m2ts.m2ts < out.pes` | Valid M2TS |
| 8.3 | Stdio → null | `freemkv stdio:// null:// < out.pes` | Exit 0 |
| 8.4 | Stdio → network | receiver: `freemkv network://0.0.0.0:9000 mkv://stdio_net.mkv`<br>sender: `freemkv stdio:// network://127.0.0.1:9000 < out.pes` | Valid MKV |

## Phase 9: Network as Input [UHD ISO] (3 min)

| # | Test | Command | Pass |
|---|------|---------|------|
| 9.1 | Network → MKV | receiver: `freemkv network://0.0.0.0:9000 mkv://net_to_mkv.mkv`<br>sender: `freemkv iso://DUNE_UHD.iso network://127.0.0.1:9000 -t 3` | Valid MKV |
| 9.2 | Network → M2TS | receiver: `freemkv network://0.0.0.0:9000 m2ts://net_to_m2ts.m2ts`<br>sender: `freemkv iso://DUNE_UHD.iso network://127.0.0.1:9000 -t 3` | Valid M2TS |
| 9.3 | Network → null | receiver: `freemkv network://0.0.0.0:9000 null://`<br>sender: `freemkv iso://DUNE_UHD.iso network://127.0.0.1:9000 -t 3` | Exit 0 |
| 9.4 | Network → stdio | receiver: `freemkv network://0.0.0.0:9000 stdio:// -q > net_out.pes`<br>sender: `freemkv iso://DUNE_UHD.iso network://127.0.0.1:9000 -t 3` | File created |

## Phase 10: Roundtrip Tests [UHD ISO] (5 min)

Prove no data loss across format conversions.

| # | Test | Pass |
|---|------|------|
| 10.1 | ISO→MKV size == M2TS→MKV size | File sizes match (±1%) |
| 10.2 | ISO→M2TS→MKV == ISO→MKV | MKV sizes match |
| 10.3 | ISO→MKV→M2TS→MKV | Third MKV matches first MKV |
| 10.4 | ISO→network→MKV == ISO→MKV | MKV sizes match |
| 10.5 | ISO→stdio→MKV == ISO→MKV | MKV sizes match |
| 10.6 | ffprobe all outputs | All exit 0, correct codecs |
| 10.7 | ffmpeg decode all outputs 30s | <10 errors each |

## Phase 11: Batch Mode [UHD ISO] (~30 min)

| # | Test | Command | Pass |
|---|------|---------|------|
| 11.1 | All titles | `freemkv iso://DUNE_UHD.iso mkv:///data/test/batch/` | 7 MKV files |
| 11.2 | Multi select | `freemkv iso://DUNE_UHD.iso mkv://multi/ -t 2 -t 3` | 2 MKV files |

## Phase 12: ISO → ISO Decrypt [UHD ISO] (~20 min)

| # | Test | Command | Pass |
|---|------|---------|------|
| 12.1 | Decrypt ISO | `freemkv iso://DUNE_UHD.iso iso://DUNE_DEC.iso` | Same size, decrypted |

## Phase 13: Direct Disc Rip [UHD] (~30 min)

| # | Test | Command | Pass |
|---|------|---------|------|
| 13.1 | Disc → MKV single | `freemkv disc:// mkv://disc_t1.mkv -t 1` | Valid MKV |
| 13.2 | Disc → MKV batch | `freemkv disc:// mkv:///data/test/disc_all/` | Batch MKVs |
| 13.3 | Disc → M2TS | `freemkv disc:// m2ts://disc_t1.m2ts -t 1` | Valid M2TS |
| 13.4 | Disc → null | `freemkv disc:// null:// -t 1` | Speed reported |
| 13.5 | Disc → network | receiver: `freemkv network://0.0.0.0:9000 mkv://disc_net.mkv`<br>sender: `freemkv disc:// network://127.0.0.1:9000 -t 3` | Valid MKV |
| 13.6 | Disc → stdio | `freemkv disc:// stdio:// -t 3 -q > disc_out.pes` | File created |

## Phase 14: Edge Cases [UHD ISO] (2 min)

| # | Test | Command | Pass |
|---|------|---------|------|
| 14.1 | Very short title | `freemkv iso://DUNE_UHD.iso mkv://short.mkv -t 5` | Completes, file > 0 |
| 14.2 | --raw ISO→MKV | `freemkv iso://DUNE_UHD.iso mkv://raw.mkv --raw -t 3` | Encrypted MKV (garbled but no crash) |
| 14.3 | Quiet mode | `freemkv iso://DUNE_UHD.iso mkv://q.mkv -t 3 -q` | Zero stdout/stderr |

---

## — SWAP DISC: Insert BD —

## Phase 15: BD Disc Info [BD] (1 min)

| # | Test | Command | Pass |
|---|------|---------|------|
| 15.1 | Disc info | `freemkv info disc://` | Titles, H.264, DTS-HD MA |

## Phase 16: BD ISO Rip [BD] (~varies)

| # | Test | Command | Pass |
|---|------|---------|------|
| 16.1 | Disc → ISO raw | `freemkv disc:// iso://BD.iso --raw` | Full BD ISO |

## Phase 17: BD Stream Tests [BD ISO] (~20 min)

| # | Test | Command | Pass |
|---|------|---------|------|
| 17.1 | ISO → MKV | `freemkv iso://BD.iso mkv://bd_t1.mkv -t 1` | H.264 MKV |
| 17.2 | ISO → M2TS | `freemkv iso://BD.iso m2ts://bd_t1.m2ts -t 1` | Decrypted M2TS |
| 17.3 | ISO → null | `freemkv iso://BD.iso null:// -t 1` | Speed report |
| 17.4 | ISO → stdio | `freemkv iso://BD.iso stdio:// -t 1 -q > bd_out.pes` | File created |
| 17.5 | ISO → network | receiver: `freemkv network://0.0.0.0:9000 mkv://bd_net.mkv`<br>sender: `freemkv iso://BD.iso network://127.0.0.1:9000 -t 1` | Valid MKV |
| 17.6 | M2TS → MKV | `freemkv m2ts://bd_t1.m2ts mkv://bd_remux.mkv` | Valid MKV |
| 17.7 | M2TS → M2TS | `freemkv m2ts://bd_t1.m2ts m2ts://bd_m2ts_remux.m2ts` | Similar size |
| 17.8 | M2TS → null | `freemkv m2ts://bd_t1.m2ts null://` | Speed report |
| 17.9 | M2TS → stdio | `freemkv m2ts://bd_t1.m2ts stdio:// -q > bd_m2ts_out.pes` | File created |
| 17.10 | M2TS → network | receiver: `freemkv network://0.0.0.0:9000 mkv://bd_m2ts_net.mkv`<br>sender: `freemkv m2ts://bd_t1.m2ts network://127.0.0.1:9000` | Valid MKV |
| 17.11 | MKV → M2TS | `freemkv mkv://bd_t1.mkv m2ts://bd_mkv_to_m2ts.m2ts` | Valid M2TS |
| 17.12 | MKV → MKV | `freemkv mkv://bd_t1.mkv mkv://bd_mkv_remux.mkv` | Same size |
| 17.13 | MKV → null | `freemkv mkv://bd_t1.mkv null://` | Speed report |
| 17.14 | MKV → stdio | `freemkv mkv://bd_t1.mkv stdio:// -q > bd_mkv_out.pes` | File created |
| 17.15 | MKV → network | receiver: `freemkv network://0.0.0.0:9000 mkv://bd_mkv_net.mkv`<br>sender: `freemkv mkv://bd_t1.mkv network://127.0.0.1:9000` | Valid MKV |
| 17.16 | Batch | `freemkv iso://BD.iso mkv://bd_batch/` | Multiple MKVs |
| 17.17 | ffprobe MKV | `ffprobe bd_t1.mkv` | H.264 1080p, DTS-HD MA |
| 17.18 | ffmpeg decode | `ffmpeg -v error -i bd_t1.mkv -t 30 -f null -` | <10 errors |

---

## — SWAP DISC: Insert DVD —

## Phase 18: DVD Disc Info [DVD] (1 min)

| # | Test | Command | Pass |
|---|------|---------|------|
| 18.1 | Disc info | `freemkv info disc://` | Titles, MPEG-2, AC-3 |

## Phase 19: DVD ISO Rip [DVD] (~varies)

| # | Test | Command | Pass |
|---|------|---------|------|
| 19.1 | Disc → ISO raw | `freemkv disc:// iso://DVD.iso --raw` | Full DVD ISO |

## Phase 20: DVD Stream Tests [DVD ISO] (~15 min)

| # | Test | Command | Pass |
|---|------|---------|------|
| 20.1 | ISO → MKV | `freemkv iso://DVD.iso mkv://dvd_t1.mkv -t 1` | MPEG-2 MKV |
| 20.2 | ISO → M2TS | `freemkv iso://DVD.iso m2ts://dvd_t1.m2ts -t 1` | Decrypted M2TS |
| 20.3 | ISO → null | `freemkv iso://DVD.iso null:// -t 1` | Speed report |
| 20.4 | ISO → stdio | `freemkv iso://DVD.iso stdio:// -t 1 -q > dvd_out.pes` | File created |
| 20.5 | ISO → network | receiver: `freemkv network://0.0.0.0:9000 mkv://dvd_net.mkv`<br>sender: `freemkv iso://DVD.iso network://127.0.0.1:9000 -t 1` | Valid MKV |
| 20.6 | M2TS → MKV | `freemkv m2ts://dvd_t1.m2ts mkv://dvd_remux.mkv` | Valid MKV |
| 20.7 | M2TS → M2TS | `freemkv m2ts://dvd_t1.m2ts m2ts://dvd_m2ts_remux.m2ts` | Similar size |
| 20.8 | M2TS → null | `freemkv m2ts://dvd_t1.m2ts null://` | Speed report |
| 20.9 | M2TS → stdio | `freemkv m2ts://dvd_t1.m2ts stdio:// -q > dvd_m2ts_out.pes` | File created |
| 20.10 | M2TS → network | receiver: `freemkv network://0.0.0.0:9000 mkv://dvd_m2ts_net.mkv`<br>sender: `freemkv m2ts://dvd_t1.m2ts network://127.0.0.1:9000` | Valid MKV |
| 20.11 | MKV → M2TS | `freemkv mkv://dvd_t1.mkv m2ts://dvd_mkv_to_m2ts.m2ts` | Valid M2TS |
| 20.12 | MKV → MKV | `freemkv mkv://dvd_t1.mkv mkv://dvd_mkv_remux.mkv` | Same size |
| 20.13 | MKV → null | `freemkv mkv://dvd_t1.mkv null://` | Speed report |
| 20.14 | MKV → stdio | `freemkv mkv://dvd_t1.mkv stdio:// -q > dvd_mkv_out.pes` | File created |
| 20.15 | MKV → network | receiver: `freemkv network://0.0.0.0:9000 mkv://dvd_mkv_net.mkv`<br>sender: `freemkv mkv://dvd_t1.mkv network://127.0.0.1:9000` | Valid MKV |
| 20.16 | Batch | `freemkv iso://DVD.iso mkv://dvd_batch/` | Multiple MKVs |
| 20.17 | ffprobe MKV | `ffprobe dvd_t1.mkv` | MPEG-2 480i/576i, AC-3 |
| 20.18 | ffmpeg decode | `ffmpeg -v error -i dvd_t1.mkv -t 30 -f null -` | <10 errors |
| 20.19 | DVD disc → MKV | `freemkv disc:// mkv://dvd_disc.mkv -t 1` | MPEG-2 MKV (CSS decrypted) |
| 20.20 | DVD disc → M2TS | `freemkv disc:// m2ts://dvd_disc.m2ts -t 1` | Valid M2TS |
| 20.21 | DVD disc → null | `freemkv disc:// null:// -t 1` | Speed report |
| 20.22 | DVD disc → stdio | `freemkv disc:// stdio:// -t 1 -q > dvd_disc_out.pes` | File created |

---

## Automated Tests (cargo test)

| # | Test Suite | Command | Pass |
|---|------------|---------|------|
| A.1 | CLI integration | `cd freemkv && cargo test` | 9/9 |
| A.2 | Library unit | `cd libfreemkv && cargo test` | 41+/41+ |
| A.3 | Clippy clean | `cargo clippy -- -W clippy::all` | No errors |

---

## Stream Matrix

Every input × every output. All cells covered.

| Input ↓ / Output → | MKV | M2TS | Network | Stdio | Null |
|---------------------|:---:|:----:|:-------:|:-----:|:----:|
| ISO (UHD) | 4.1 | 4.3 | 4.6 | 4.5 | 4.4 |
| ISO (BD) | 17.1 | 17.2 | 17.5 | 17.4 | 17.3 |
| ISO (DVD) | 20.1 | 20.2 | 20.5 | 20.4 | 20.3 |
| Disc (UHD) | 13.1 | 13.3 | 13.5 | 13.6 | 13.4 |
| Disc (DVD) | 20.19 | 20.20 | — | 20.22 | 20.21 |
| M2TS (UHD) | 6.1 | 6.2 | 6.4 | 6.6 | 6.3 |
| M2TS (BD) | 17.6 | 17.7 | 17.10 | 17.9 | 17.8 |
| M2TS (DVD) | 20.6 | 20.7 | 20.10 | 20.9 | 20.8 |
| MKV (UHD) | 7.2 | 7.1 | 7.5 | 7.6 | 7.3 |
| MKV (BD) | 17.12 | 17.11 | 17.15 | 17.14 | 17.13 |
| MKV (DVD) | 20.12 | 20.11 | 20.15 | 20.14 | 20.13 |
| Network | 9.1 | 9.2 | — | 9.4 | 9.3 |
| Stdio | 8.1 | 8.2 | 8.4 | — | 8.3 |

## Summary

| Phase | Tests | Time | Disc |
|-------|-------|------|------|
| 1. Errors | 10 | 30s | None |
| 2. Info | 6 | 1m | UHD |
| 3. ISO rip | 3 | 100m | UHD |
| 4. ISO→outputs | 10 | 15m | ISO |
| 5. Verify | 7 | 2m | ISO |
| 6. M2TS input | 6 | 5m | ISO |
| 7. MKV input | 6 | 5m | ISO |
| 8. Stdio input | 4 | 3m | ISO |
| 9. Network input | 4 | 3m | ISO |
| 10. Roundtrips | 7 | 5m | ISO |
| 11. Batch | 2 | 30m | ISO |
| 12. ISO decrypt | 1 | 20m | ISO |
| 13. Direct disc | 6 | 30m | UHD |
| 14. Edge cases | 3 | 2m | ISO |
| 15. BD info | 1 | 1m | BD |
| 16. BD ISO | 1 | varies | BD |
| 17. BD streams | 18 | 20m | BD ISO |
| 18. DVD info | 1 | 1m | DVD |
| 19. DVD ISO | 1 | varies | DVD |
| 20. DVD streams | 22 | 15m | DVD ISO |
| A. Automated | 3 | 30s | None |
| | **Total** | **122** | **~6 hrs** |

## Cleanup

```bash
# Keep ISOs as fixtures, plus one MKV + M2TS per disc type
rm -f /data/test/*_v.* /data/test/*_q.* /data/test/raw_check.iso
rm -f /data/test/remux.* /data/test/net.* /data/test/pipe.*
rm -f /data/test/DUNE_DEC.iso /data/test/out.mkv /data/test/out.pes
rm -f /data/test/short.mkv /data/test/raw.mkv /data/test/q.mkv
rm -f /data/test/quiet.mkv /data/test/verbose.mkv
rm -f /data/test/m2ts_to_mkv.mkv /data/test/m2ts_remux.m2ts
rm -f /data/test/mkv_to_m2ts.m2ts /data/test/mkv_remux.mkv
rm -rf /data/test/batch /data/test/multi /data/test/disc_all
rm -rf /data/test/bd_batch /data/test/dvd_batch
```
