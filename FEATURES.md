# freemkv — Feature List

## v0.6.0 (current)

### Done
- [x] `freemkv drive-info` — display drive hardware and profile match
- [x] `freemkv drive-info --share` — capture and submit drive profile to GitHub
- [x] `freemkv drive-info --mask` — mask serial numbers for privacy
- [x] `freemkv disc-info` — show disc titles, streams, sizes, labels
- [x] `freemkv disc-info --full` — show all titles (not just top 5)
- [x] `freemkv disc-info --basic` — show disc info without BD-J labels
- [x] `freemkv rip` — full disc backup with AACS decryption
- [x] MKV output (default) with native muxer
- [x] m2ts output via `--raw` flag
- [x] `freemkv remux` — convert m2ts to MKV without a drive
- [x] `freemkv update-keys` — download and update KEYDB.cfg
- [x] Title selection (`--title N`) and listing (`--list`)
- [x] Auto-detect BD drives on /dev/sg0-15
- [x] Progress display: speed, ETA, percentage
- [x] SIGINT handling: clean interrupt, disc ejected
- [x] Adaptive error handling: batch ramp-down, speed reduction
- [x] i18n string table (en + es bundled, runtime locale loading)
- [x] Safe output filenames (spaces → underscores)
- [x] Works with all drives (profile match optional)
- [x] Stream labels from 5 BD-J format parsers
- [x] Platform detection: MediaTek MT1959 (A + B variants)
- [x] Captures 15 GET_CONFIG features for profile sharing
- [x] Auto-generates drive.toml with feature mapping
- [x] Profile submission via GitHub API

### Planned
- [ ] `--json` output format
- [ ] Resume interrupted rips
- [ ] Interactive title selection
- [ ] Pioneer Renesas platform support
- [ ] macOS native support (IOKit backend)
- [ ] Windows support (SPTI backend)
- [ ] DVD CSS decryption
