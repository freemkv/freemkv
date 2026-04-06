# freemkv — Feature List

## v0.1.0 (current)

### Done
- [x] `freemkv info` — display drive information
- [x] `freemkv info --share` — capture bdemu-compatible profile
- [x] `freemkv info --mask` — format-preserving serial masking
- [x] Auto-detect BD drives on /dev/sg0-15
- [x] Platform detection: Pioneer (RB 0xF1), MTK (RB mode 6)
- [x] Captures 15 GET_CONFIG features
- [x] Captures MODE_SENSE, REPORT_KEY, vendor READ_BUFFER
- [x] Auto-generates drive.toml with feature mapping
- [x] Submit link to GitHub Issues for profile sharing

### v0.2.0 (planned)
- [ ] `freemkv rip` — disc backup
- [ ] Identity fingerprint computation (SHA1)
- [ ] Keys database lookup: supported / not supported / unknown
- [ ] `--json` output format
- [ ] HL-DT-ST Renesas platform detection (RB mode 5)
- [ ] Automated profile submission via GitHub API

### v0.3.0 (future)
- [ ] `freemkv rip` — full disc backup with libfreemkv
- [ ] Title selection and metadata display
- [ ] Progress reporting
- [ ] Resume interrupted rips
- [ ] Windows support
- [ ] macOS support
