#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TmdbResult {
    pub title: String,
    pub year: u16,
    pub poster_url: String,
    pub overview: String,
    pub media_type: String, // "movie" or "tv"
    /// TMDB numeric id for this match (0 = unknown). Carried so a consumer —
    /// autorip's own metadata enrichment, or kdb resolving a volume id via
    /// [`lookup`] — can fetch anything else it wants straight from TMDB by id,
    /// without re-searching. `serde(default)` so markers written before this
    /// field existed still deserialize.
    #[serde(default)]
    pub tmdb_id: u64,
}

// Shared agent for all TMDB calls: ureq sets NO connect/read timeout by default, so a hung
// connection would wedge the rip thread or a web handler indefinitely.
static AGENT: once_cell::sync::Lazy<ureq::Agent> = once_cell::sync::Lazy::new(|| {
    let config = ureq::config::Config::builder()
        .timeout_connect(Some(std::time::Duration::from_secs(5)))
        .timeout_recv_response(Some(std::time::Duration::from_secs(10)))
        // Follow NO redirects: the URL carries the operator's api_key in its
        // query string, so a 3xx (TMDB compromised/tampered on-path) would
        // hand that key to an arbitrary host; `fetch_multi` reports it as "no result".
        .max_redirects(0)
        .build();
    ureq::Agent::new_with_config(config)
});

// Build the `search/multi` URL. Both `api_key` and `query` are percent-encoded:
// a stray space/&/#/= in a copy-pasted key would otherwise yield a malformed
// or silently-wrong URL, and `query` is untrusted disc-label content.
fn search_multi_url(query: &str, api_key: &str) -> String {
    format!(
        "https://api.themoviedb.org/3/search/multi?api_key={}&query={}&page=1",
        urlencoded(api_key),
        urlencoded(query)
    )
}

// Cap on the TMDB response body we'll buffer. A real `search/multi` response
// is tens of KB; 2 MiB is generous headroom. Bounding it stops a hostile or
// broken endpoint from streaming an unbounded body into memory (DoS).
const MAX_TMDB_BYTES: u64 = 2 * 1024 * 1024;

// Read at most `cap` bytes, rejecting anything over: an oversized body reads `cap+1` bytes
// successfully then fails the boundary check below, rather than being silently truncated.
fn read_capped_bytes(reader: impl std::io::Read, cap: u64) -> std::io::Result<Vec<u8>> {
    use std::io::Read as _;
    let mut buf = Vec::new();
    reader.take(cap + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > cap {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "tmdb response exceeded size cap",
        ));
    }
    Ok(buf)
}

/// Read at most `MAX_TMDB_BYTES` from the response body, rejecting anything
/// over the cap, then parse as JSON. Replaces `resp.into_json()`, which reads
/// the whole body with no upper bound.
fn read_capped_json(resp: ureq::http::Response<ureq::Body>) -> std::io::Result<serde_json::Value> {
    let buf = read_capped_bytes(resp.into_body().into_reader(), MAX_TMDB_BYTES)?;
    serde_json::from_slice(&buf)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

// Run a `search/multi` request via the shared timeout-bounded AGENT. Splits
// out 401 (bad key, throttled warning) from other status/transport errors
// instead of collapsing every failure to "no results", hiding the cause.
fn fetch_multi(query: &str, api_key: &str) -> Option<serde_json::Value> {
    let url = search_multi_url(query, api_key);
    match AGENT.get(&url).call() {
        Ok(resp) => match read_capped_json(resp) {
            Ok(json) => Some(json),
            Err(e) => {
                tracing::warn!(query = %query, error = %e, "tmdb: response was not valid JSON");
                None
            }
        },
        Err(ureq::Error::StatusCode(401)) => {
            warn_bad_key_throttled();
            None
        }
        Err(ureq::Error::StatusCode(code)) => {
            tracing::warn!(query = %query, status = code, "tmdb: HTTP error status");
            None
        }
        Err(e) => {
            // Do NOT log `e` directly: the URL has the api_key in its query
            // string, and `BadUri`'s Display still prints the rejected URI,
            // so masking stays even though ureq 3 is URL-free elsewhere.
            let error_kind = crate::web::ureq_error_kind(&e);
            tracing::warn!(query = %query, error_kind = %error_kind, "tmdb: request failed (network/transport)");
            None
        }
    }
}

/// One-per-minute warning that the configured TMDB API key was rejected.
fn warn_bad_key_throttled() {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static LAST_WARN_SECS: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let last = LAST_WARN_SECS.load(Ordering::Relaxed);
    if now.saturating_sub(last) >= 60
        && LAST_WARN_SECS
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        tracing::warn!(
            "tmdb: API key rejected (HTTP 401) — check the TMDB_API_KEY in Settings; \
             titles will fall through to the needs-review queue until it is fixed"
        );
        crate::log::syslog("TMDB API key rejected (HTTP 401) — check TMDB_API_KEY in Settings");
    }
}

/// Resolve a disc `label` to a TMDB movie/TV entry.
///
/// Takes the RAW disc volume label (not a pre-cleaned string): cleaning and
/// progressive-fallback trimming both live here so the lookup and the auto-file gate
/// ([`is_confident_match`]) never disagree on what was searched. Queries the cleaned label,
/// then on no confident match peels junk-shaped trailing tokens one at a time and re-queries
/// (`query_variants`). Returns the first confident match (exact title + year) across the
/// variants, else the best non-exact guess.
pub fn lookup(label: &str, api_key: &str) -> Option<TmdbResult> {
    if api_key.is_empty() {
        return None;
    }
    // A separator-only label yields no query variants; short-circuit rather
    // than firing `query=&...` (TMDB answers HTTP 422). A season marker
    // ("… Season 5") means TV — bias the pick so it can't be outranked.
    let prefer_tv = season_from_label(label).is_some();
    let mut fallback: Option<TmdbResult> = None;
    for variant in query_variants(label) {
        if variant.trim().is_empty() {
            continue;
        }
        let Some(resp) = fetch_multi(&variant, api_key) else {
            continue;
        };
        let Some(results) = resp["results"].as_array() else {
            continue;
        };
        if let Some(best) = pick_best(&variant, results, prefer_tv) {
            // Confident = exact normalized title match on THIS variant + a year.
            if best.year > 0 && norm(&best.title) == norm(&variant) {
                return Some(best);
            }
            if fallback.is_none() {
                fallback = Some(best);
            }
        }
    }
    fallback
}

/// Normalize a title for comparison: lowercase, every run of non-alphanumerics
/// collapses to one space, trimmed. So "Top Gun: Maverick" and the disc label
/// "Top Gun Maverick" both become "top gun maverick".
fn norm(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut sep = true; // leading: suppress a leading space
    for c in s.chars() {
        // Unicode-aware: keep accented letters/digits (so "Amélie" and
        // "Pokémon" can match exactly) instead of stripping all non-ASCII.
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
            sep = false;
        } else if !sep {
            out.push(' ');
            sep = true;
        }
    }
    out.trim_end().to_string()
}

/// Is the resolved `title`/`year` a CONFIDENT match for the disc `label`?
///
/// Confident = the title carries a year AND exactly matches (normalized) the cleaned label OR
/// any of the same progressively-trimmed variants that [`lookup`] searches. Takes the RAW label
/// (cleaning happens inside), matching `lookup`. Rips whose match is NOT confident (or that
/// would overwrite an existing file) are held for operator review rather than auto-filed under
/// a guessed name.
pub fn is_confident_match(label: &str, title: &str, year: u16) -> bool {
    year > 0 && query_variants(label).iter().any(|v| norm(v) == norm(title))
}

/// Return up to `limit` candidate matches for `query`, best first (exact dated
/// title → dated → popularity). Powers the "needs review" correction picker.
pub fn search(query: &str, api_key: &str, limit: usize) -> Vec<TmdbResult> {
    if api_key.is_empty() || query.trim().is_empty() {
        return Vec::new();
    }
    let Some(json) = fetch_multi(query, api_key) else {
        return Vec::new();
    };
    let Some(results) = json["results"].as_array() else {
        return Vec::new();
    };
    rank_search_results(query, results, limit)
}

// The pure ranking half of `search`: parse every movie/tv entry, sort exact-dated-match first,
// dated second, popularity as tiebreaker, cap at `limit`.
fn rank_search_results(
    query: &str,
    results: &[serde_json::Value],
    limit: usize,
) -> Vec<TmdbResult> {
    let want = norm(query);
    let mut parsed: Vec<(TmdbResult, f64, bool)> = results
        .iter()
        .filter_map(parse_result)
        .map(|(r, pop)| {
            let exact = r.year > 0 && norm(&r.title) == want;
            (r, pop, exact)
        })
        .collect();
    parsed.sort_by(|a, b| {
        b.2.cmp(&a.2) // exact first
            .then((b.0.year > 0).cmp(&(a.0.year > 0))) // then dated
            .then(b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)) // then popularity
    });
    parsed.into_iter().take(limit).map(|(r, _, _)| r).collect()
}

// Choose the best entry from a TMDB `search/multi` response: keep only movie/TV entries, prefer
// ones with a release year, break ties on popularity.
fn pick_best(query: &str, results: &[serde_json::Value], prefer_tv: bool) -> Option<TmdbResult> {
    let want = norm(query);
    // Ranking key, lexicographic, highest wins: (exact, dated, tv_preferred,
    // popularity). `exact` beats popularity (else generic "Undertow" matches
    // the more popular 2016 film); `tv_preferred` only breaks equal ties.
    let key = |cand: &TmdbResult, pop: f64| {
        let exact = cand.year > 0 && !want.is_empty() && norm(&cand.title) == want;
        (
            exact,
            cand.year > 0,
            prefer_tv && cand.media_type == "tv",
            pop,
        )
    };
    let mut best: Option<(TmdbResult, (bool, bool, bool, f64))> = None;
    for v in results {
        let Some((cand, popularity)) = parse_result(v) else {
            continue;
        };
        let k = key(&cand, popularity);
        let better = best.as_ref().is_none_or(|(_, bk)| key_gt(k, *bk));
        if better {
            best = Some((cand, k));
        }
    }
    best.map(|(r, _)| r)
}

/// Lexicographic "is `a` a better rank than `b`" for [`pick_best`]'s key. Hand
/// rolled because the key ends in an `f64` (popularity), which is not `Ord`.
fn key_gt(a: (bool, bool, bool, f64), b: (bool, bool, bool, f64)) -> bool {
    if a.0 != b.0 {
        return a.0;
    }
    if a.1 != b.1 {
        return a.1;
    }
    if a.2 != b.2 {
        return a.2;
    }
    a.3 > b.3
}

/// Parse one `search/multi` result into a `TmdbResult` + its popularity.
/// Returns `None` for non-movie/TV entries (people, collections) and for
/// entries missing a usable title.
fn parse_result(v: &serde_json::Value) -> Option<(TmdbResult, f64)> {
    // Default to "" (not "movie") so an entry that is missing media_type is
    // rejected by the guard below rather than silently admitted as a movie.
    let media_type = v["media_type"].as_str().unwrap_or("");
    if media_type != "movie" && media_type != "tv" {
        return None;
    }
    let title = v
        .get(if media_type == "tv" { "name" } else { "title" })?
        .as_str()?
        .to_string();
    if title.is_empty() {
        return None;
    }
    let date = v
        .get(if media_type == "tv" {
            "first_air_date"
        } else {
            "release_date"
        })
        .and_then(|d| d.as_str())
        .unwrap_or("");
    let year: u16 = date.get(..4).and_then(|y| y.parse().ok()).unwrap_or(0);
    // TMDB poster_path is always a host-absolute path ("/abc.jpg"). Guard
    // the leading slash so a slashless or unexpected value can't produce a
    // malformed/host-relative image URL — keeps the empty-path behavior.
    let poster = v["poster_path"]
        .as_str()
        .filter(|p| p.starts_with('/'))
        .map(|p| format!("https://image.tmdb.org/t/p/w300{p}"))
        .unwrap_or_default();
    let overview = v["overview"].as_str().unwrap_or("").to_string();
    let tmdb_id = v["id"].as_u64().unwrap_or(0);
    Some((
        TmdbResult {
            title,
            year,
            poster_url: poster,
            overview,
            media_type: media_type.to_string(),
            tmdb_id,
        },
        v["popularity"].as_f64().unwrap_or(0.0),
    ))
}

// Remove a parenthesized 4-digit release year, e.g. "Drive (2011)" -> "Drive ". A BARE year is
// left untouched ("Blade Runner 2049"). Char-based so a multibyte label never panics.
fn strip_paren_year(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        // Match "(dddd)" exactly: '(' then 4 ASCII digits then ')'.
        if chars[i] == '('
            && i + 5 < chars.len()
            && chars[i + 5] == ')'
            && chars[i + 1..i + 5].iter().all(|c| c.is_ascii_digit())
        {
            i += 6; // skip the whole "(dddd)" group
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// Clean a disc label for TMDB search: "AURORA_DRIFT_TWO" -> "Aurora Drift Two"
/// Strips common disc suffixes like "4K Ultra HD", "Blu-ray", "DVD", etc., and
/// a parenthesized release year (see `strip_paren_year`).
pub fn clean_title(label: &str) -> String {
    let s = label.replace(['_', '-'], " ");
    let s = strip_paren_year(&s);

    // Strip common disc format suffixes (case-insensitive)
    let suffixes = [
        "4k ultra hd",
        "4k uhd",
        "ultra hd",
        "blu ray",
        "bluray",
        "dvd",
        "disc 1",
        "disc 2",
        "disc 3",
        "disc 4",
        "disk 1",
        "disk 2",
        "disk 3",
        "disk 4",
    ];
    // Search AND slice the SAME (lowercased) string: `to_lowercase()` can
    // change byte length, so an offset in `lower` may not index into `s`.
    // Strip only END-anchored suffixes, never embedded; repeat to peel groups.
    let lower = s.to_lowercase();
    let mut clipped = lower.as_str();
    loop {
        // Trim trailing whitespace AND non-alphanumeric junk (™/®, punctuation)
        // before testing the END-anchor: retail labels carry such trailing
        // chars ("Ultra HD™"), and whitespace-only trimming left it un-anchored.
        let trimmed = clipped.trim_end_matches(|c: char| !c.is_alphanumeric());
        let mut next: Option<&str> = None;
        for suffix in &suffixes {
            if let Some(pos) = trimmed.rfind(suffix)
                && pos + suffix.len() == trimmed.len()
            {
                next = Some(&trimmed[..pos]);
                break;
            }
        }
        // Also peel a trailing season marker ("Season 5") so a series disc
        // resolves to the base show title, only when no format suffix
        // matched this round — the two peels interleave across the loop.
        if next.is_none() {
            next = strip_trailing_season(trimmed);
        }
        match next {
            Some(rest) => clipped = rest,
            None => {
                clipped = trimmed;
                break;
            }
        }
    }
    let trimmed = clipped.trim();

    trimmed
        .split_whitespace()
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                None => String::new(),
                Some(c) => c.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// Unambiguous TV season markers. A trailing "<word> <number>" of one of these is peeled by
// `clean_title` so a series disc resolves to the base show title.
const SEASON_WORDS: &[&str] = &["season", "series", "saison", "staffel", "seizoen"];

// If `s` (lowercased, trailing-junk-trimmed) ends with a season marker like
// "season 5", return `s` with that marker removed; else None. The number is
// required — a bare trailing "season" is left alone (e.g. "Silly Season").
fn strip_trailing_season(s: &str) -> Option<&str> {
    let digits_start = s.trim_end_matches(|c: char| c.is_ascii_digit());
    if digits_start.len() == s.len() {
        return None; // no trailing digits
    }
    let head = digits_start.trim_end(); // strip the space(s) before the number
    for word in SEASON_WORDS {
        if let Some(pos) = head.rfind(word)
            && pos + word.len() == head.len()
            // Must be a whole word: start of string or a non-alphanumeric before it.
            && (pos == 0 || !head[..pos].ends_with(|c: char| c.is_alphanumeric()))
        {
            return Some(head[..pos].trim_end());
        }
    }
    None
}

/// Parse a TV season number from a disc `label`: "Longacre Season 5 Disc 2"
/// → 5, "GAMEOFTHRONES_S3_DISC1" → 3. Returns 1..=99, else `None`.
///
/// A season marker is the signal that a disc is TV rather than a film, and the
/// number is what the mover uses to place the rip under `Show (Year)/Season NN/`.
/// Recognizes the spelled-out `SEASON_WORDS` followed by a number, or a
/// compact `S<n>` token.
pub fn season_from_label(label: &str) -> Option<u16> {
    number_after_word(label, SEASON_WORDS).or_else(|| compact_token_number(label, &["s"]))
}

/// Parse a disc number from a `label`: "… Disc 2" → 2, "GOT_S3_D4" → 4,
/// "BD1" → 1. Returns 1..=99, else `None`. Used to sequence a multi-disc set.
pub fn disc_from_label(label: &str) -> Option<u16> {
    number_after_word(label, &["disc", "disk"])
        .or_else(|| compact_token_number(label, &["disc", "disk", "bd", "d"]))
}

/// The number immediately following any of `words` in `s` (case-insensitive,
/// tolerating `_ - . :` separators): "Season 5" → 5, "series2" → 2. 1..=99.
fn number_after_word(s: &str, words: &[&str]) -> Option<u16> {
    let low = s.to_lowercase();
    for word in words {
        let mut from = 0;
        while let Some(rel) = low[from..].find(word) {
            let pos = from + rel;
            let digits: String = low[pos + word.len()..]
                .trim_start_matches([' ', '_', '-', '.', ':'])
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if let Ok(n) = digits.parse::<u16>()
                && (1..=99).contains(&n)
            {
                return Some(n);
            }
            from = pos + word.len();
        }
    }
    None
}

/// A compact standalone token `<prefix><digits>` — "S3", "D2", "BD1", "DISC1".
fn compact_token_number(s: &str, prefixes: &[&str]) -> Option<u16> {
    for tok in s.split([' ', '_', '-', '.', ':']) {
        let low = tok.to_lowercase();
        for pfx in prefixes {
            if let Some(rest) = low.strip_prefix(pfx)
                && !rest.is_empty()
                && rest.chars().all(|c| c.is_ascii_digit())
                && let Ok(n) = rest.parse::<u16>()
                && (1..=99).contains(&n)
            {
                return Some(n);
            }
        }
    }
    None
}

// Edition / release qualifiers that annotate a cut but are not part of the
// film's TMDB title ("Ultimate Edition", "Director's Cut"). Peeled only when
// TRAILING (see `is_trailing_junk`) so an interior word is never removed.
const EDITION_WORDS: &[&str] = &[
    "ultimate",
    "extended",
    "theatrical",
    "director",
    "directors",
    "special",
    "collector",
    "collectors",
    "anniversary",
    "final",
    "unrated",
    "remastered",
    "limited",
    "deluxe",
    "steelbook",
    "edition",
    "cut",
    "version",
];

/// Region / market codes that retail volume labels append.
const REGION_WORDS: &[&str] = &["uk", "usa", "us", "eu", "na", "ww", "aus", "region"];

/// Disc-format words. `clean_title` already strips multi-word variants
/// ("4k ultra hd", "blu ray"); these single tokens are what a trailing-token
/// peel sees ("BD", "UHD", "3D"-style tokens are caught by the alnum rule).
const FORMAT_WORDS: &[&str] = &[
    "bd", "bdrom", "uhd", "4k", "hd", "sd", "dvd", "bluray", "video",
];

/// Is `s` (lowercased) composed only of roman-numeral letters? Used only to
/// PREVENT trimming a trailing roman numeral ("Rocky II"): a false positive on
/// a real word like "mix" merely keeps that token, which is always safe.
fn is_roman_numeral(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| matches!(c, 'i' | 'v' | 'x' | 'l' | 'c' | 'd' | 'm'))
}

// May this TRAILING token be safely peeled off a disc label? Edition/region/ format words and
// obvious codes — but NEVER a bare number or roman numeral, which are sequel markers ("Alien
// 3", "Rocky II").
fn is_trailing_junk(tok: &str) -> bool {
    if tok.is_empty() {
        return false;
    }
    // Sequel markers are meaningful — never peel.
    if tok.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    let low = tok.to_lowercase();
    if is_roman_numeral(&low) {
        return false;
    }
    if EDITION_WORDS.contains(&low.as_str())
        || REGION_WORDS.contains(&low.as_str())
        || FORMAT_WORDS.contains(&low.as_str())
    {
        return true;
    }
    // Mixed-alphanumeric packaging/format code: BD3, UPT1, G51, D2, 3D, 4K.
    let has_alpha = tok.chars().any(|c| c.is_ascii_alphabetic());
    let has_digit = tok.chars().any(|c| c.is_ascii_digit());
    if has_alpha && has_digit {
        return true;
    }
    // Short all-caps abbreviation (not roman numeral): UE, SE, EDC, WW. Case
    // is preserved here since variants are peeled BEFORE `clean_title`
    // title-cases, so "UE" (junk) stays distinguishable from "Us" (real title).
    tok.len() <= 4 && tok.chars().all(|c| c.is_ascii_uppercase())
}

// Progressively-trimmed TMDB query variants for a disc `label`, most specific first. Variant 0
// is `clean_title(label)`; each next variant peels one more junk-shaped trailing token
// (`is_trailing_junk`).
fn query_variants(label: &str) -> Vec<String> {
    const MAX_QUERY_VARIANTS: usize = 5;
    // Same separator/year normalization clean_title applies, but WITHOUT the
    // case folding, so junk detection keeps the label's original casing.
    let base = label.replace(['_', '-'], " ");
    let base = strip_paren_year(&base);
    let toks: Vec<&str> = base.split_whitespace().collect();
    let mut variants: Vec<String> = Vec::new();
    let mut end = toks.len();
    loop {
        let q = clean_title(&toks[..end].join(" "));
        if !q.is_empty() && !variants.iter().any(|v| v == &q) {
            variants.push(q);
        }
        if variants.len() >= MAX_QUERY_VARIANTS || end <= 1 || !is_trailing_junk(toks[end - 1]) {
            break;
        }
        end -= 1;
    }
    variants
}

fn urlencoded(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => (b as char).to_string(),
            b' ' => "+".to_string(),
            _ => format!("%{:02X}", b),
        })
        .collect()
}

// ---- TV episode resolution ------------------------------------------------

/// One episode from a TMDB season listing.
#[derive(Debug, Clone, PartialEq)]
pub struct Episode {
    pub number: u16,
    pub name: String,
    /// Runtime in minutes (0 = unknown), used to sanity-check the order-based
    /// title→episode pairing.
    pub runtime_min: u16,
}

/// The episode a ripped title is assigned to, for `Show S{NN}E{MM}` naming.
#[derive(Debug, Clone, PartialEq)]
pub struct EpisodeAssignment {
    pub episode: u16,
    /// TMDB episode name, or "" when unknown (degraded / low-confidence).
    pub name: String,
}

/// Fetch a TV season's episode list from TMDB (`GET /3/tv/{id}/season/{n}`).
///
/// Uses the same shared timeout-bounded `AGENT`, size-capped JSON read, and
/// no-redirect policy as `fetch_multi`. Returns an empty vec on ANY failure
/// (bad id, no season, network, non-JSON) — the TV auto-naming path degrades to
/// plain sequential numbering rather than blocking.
pub fn season_episodes(tv_id: u64, season: u16, api_key: &str) -> Vec<Episode> {
    if api_key.is_empty() || tv_id == 0 {
        return Vec::new();
    }
    let url = format!(
        "https://api.themoviedb.org/3/tv/{tv_id}/season/{season}?api_key={}",
        urlencoded(api_key)
    );
    match AGENT.get(&url).call() {
        Ok(resp) => match read_capped_json(resp) {
            Ok(json) => parse_episodes(&json),
            Err(e) => {
                tracing::warn!(tv_id, season, error = %e, "tmdb: season response was not valid JSON");
                Vec::new()
            }
        },
        Err(ureq::Error::StatusCode(401)) => {
            warn_bad_key_throttled();
            Vec::new()
        }
        Err(e) => {
            let error_kind = crate::web::ureq_error_kind(&e);
            tracing::warn!(tv_id, season, error_kind = %error_kind, "tmdb: season fetch failed");
            Vec::new()
        }
    }
}

/// Parse the `episodes` array of a `/tv/{id}/season/{n}` response.
fn parse_episodes(json: &serde_json::Value) -> Vec<Episode> {
    let Some(arr) = json["episodes"].as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|e| {
            let number = u16::try_from(e["episode_number"].as_u64()?).ok()?;
            Some(Episode {
                number,
                name: e["name"].as_str().unwrap_or("").to_string(),
                runtime_min: e["runtime"]
                    .as_u64()
                    .and_then(|r| u16::try_from(r).ok())
                    .unwrap_or(0),
            })
        })
        .collect()
}

/// Assign ripped titles to episodes for `Show S{NN}E{MM}` naming.
///
/// The i-th selected title (in disc order) maps to episode `start + i`. When a
/// TMDB episode of that number exists AND its runtime is plausibly the same as
/// the ripped title's, its name is attached; otherwise the assignment is still
/// numbered but nameless (degraded). `start` defaults to 1 at the call site.
///
/// Order-based by design: disc title order is broadcast order in practice, and
/// the runtime check flags gross violations without over-fitting. `title_secs`
/// is each selected title's duration in seconds, in disc order.
pub fn map_episodes(
    title_secs: &[f64],
    episodes: &[Episode],
    start: u16,
) -> Vec<EpisodeAssignment> {
    title_secs
        .iter()
        .enumerate()
        .map(|(i, &secs)| {
            let episode = start.saturating_add(i as u16);
            let name = episodes
                .iter()
                .find(|e| e.number == episode)
                .filter(|e| runtime_plausible(secs, e.runtime_min))
                .map(|e| e.name.clone())
                .unwrap_or_default();
            EpisodeAssignment { episode, name }
        })
        .collect()
}

// Is a ripped title's `secs` runtime plausibly the TMDB episode's `ep_min`
// minutes? Unknown episode runtime (0) never rejects. Tolerance is the larger
// of 5 minutes and 25% (ad breaks, PAL speed-up routinely shift runtimes).
fn runtime_plausible(secs: f64, ep_min: u16) -> bool {
    if ep_min == 0 {
        return true;
    }
    let title_min = secs / 60.0;
    let tol = (ep_min as f64 * 0.25).max(5.0);
    (title_min - ep_min as f64).abs() <= tol
}

/// Choose the starting episode number for a disc by aligning its title runtimes against the
/// TMDB season's episode runtimes (instead of counting an offset), so a disc with any
/// distinctively-timed episode is pinned to its true position regardless of how earlier discs
/// split. Returns `fallback` on any absence of signal (no episodes/runtimes, a disc that can't
/// fit, or a tie).
pub fn align_disc_offset(title_secs: &[f64], episodes: &[Episode], fallback: u16) -> u16 {
    let count = title_secs.len();
    if count == 0 || episodes.is_empty() {
        return fallback;
    }
    // The disc has to fit inside the season's episode numbering, or there is no
    // honest position to align to.
    let min_ep = episodes.iter().map(|e| e.number).min().unwrap_or(1).max(1);
    let max_ep = episodes.iter().map(|e| e.number).max().unwrap_or(0);
    let Some(last_start) = max_ep.checked_sub(count as u16 - 1) else {
        return fallback;
    };
    if last_start < min_ep {
        return fallback;
    }
    let runtime_of = |n: u16| {
        episodes
            .iter()
            .find(|e| e.number == n)
            .map(|e| e.runtime_min)
    };

    // Best = smallest average runtime distance over comparable pairs; ties
    // break toward `fallback`. An offset with no comparable pair is skipped,
    // so a season with no runtime data leaves `best_signal == 0` → `fallback`.
    let mut best_start = fallback;
    let mut best_avg = f64::INFINITY;
    let mut best_signal = 0u32;
    for start in min_ep..=last_start {
        let mut dist = 0.0f64;
        let mut signal = 0u32;
        for (i, &secs) in title_secs.iter().enumerate() {
            if secs <= 0.0 {
                continue;
            }
            if let Some(ep_min) = runtime_of(start + i as u16)
                && ep_min > 0
            {
                dist += (secs / 60.0 - ep_min as f64).abs();
                signal += 1;
            }
        }
        if signal == 0 {
            continue;
        }
        let avg = dist / signal as f64;
        let closer =
            (start as i32 - fallback as i32).abs() < (best_start as i32 - fallback as i32).abs();
        let better =
            best_signal == 0 || avg < best_avg - 1e-9 || ((avg - best_avg).abs() <= 1e-9 && closer);
        if better {
            best_start = start;
            best_avg = avg;
            best_signal = signal;
        }
    }
    if best_signal == 0 {
        return fallback;
    }
    best_start
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_title_title_cases_snake_case() {
        assert_eq!(clean_title("AURORA_DRIFT_TWO"), "Aurora Drift Two");
        assert_eq!(clean_title("K_FOR_KESTREL"), "K For Kestrel");
    }

    #[test]
    fn clean_title_strips_uhd_suffix() {
        assert_eq!(clean_title("AURORA_DRIFT_TWO_4K_UHD"), "Aurora Drift Two");
        assert_eq!(
            clean_title("AURORA_DRIFT_TWO_4K_ULTRA_HD"),
            "Aurora Drift Two"
        );
    }

    #[test]
    fn clean_title_strips_bluray_suffix() {
        assert_eq!(clean_title("THE_MATRIX_BLU_RAY"), "The Matrix");
        assert_eq!(clean_title("THE_MATRIX_BLURAY"), "The Matrix");
    }

    #[test]
    fn clean_title_strips_disc_suffix() {
        assert_eq!(clean_title("LORD_OF_THE_RINGS_DISC_1"), "Lord Of The Rings");
    }

    #[test]
    fn clean_title_handles_hyphens() {
        assert_eq!(clean_title("STAR-RANGER"), "Star Ranger");
    }

    #[test]
    fn clean_title_strips_parenthesized_year() {
        // Retail meta-titles annotate the release year in parens; leaving it in
        // the query makes TMDB return 0 hits ("Drive (2011)" matches nothing).
        assert_eq!(clean_title("Drive (2011) - 4K Ultra HD"), "Drive");
        assert_eq!(clean_title("Zombieland (2009)"), "Zombieland");
        assert_eq!(clean_title("The Matrix (1999) BLURAY"), "The Matrix");
    }

    #[test]
    fn clean_title_keeps_bare_year_in_title() {
        // A BARE (unparenthesized) year is part of the title — never stripped.
        assert_eq!(clean_title("BLADE_RUNNER_2049"), "Blade Runner 2049");
        assert_eq!(clean_title("1917"), "1917");
    }

    #[test]
    fn strip_paren_year_boundaries() {
        // Exactly "(dddd)" is removed.
        assert_eq!(strip_paren_year("Drive (2011)"), "Drive ");
        assert_eq!(strip_paren_year("(2011)"), "");
        // Near-miss digit counts are NOT a year group → kept verbatim.
        assert_eq!(strip_paren_year("(201)"), "(201)");
        assert_eq!(strip_paren_year("(20111)"), "(20111)");
        assert_eq!(strip_paren_year("(20)"), "(20)");
        // Non-digit contents kept.
        assert_eq!(strip_paren_year("(abcd)"), "(abcd)");
        assert_eq!(strip_paren_year("(20a1)"), "(20a1)");
        // Malformed / unbalanced parens kept (no crash, no partial strip).
        assert_eq!(strip_paren_year("(2011"), "(2011");
        assert_eq!(strip_paren_year("2011)"), "2011)");
        // Multiple year groups all removed.
        assert_eq!(strip_paren_year("(2001) x (2002)"), " x ");
        // Multibyte input must not panic and must strip correctly.
        assert_eq!(strip_paren_year("Amélie (2001)"), "Amélie ");
        // A 4-digit group adjacent to the end.
        assert_eq!(strip_paren_year("Se7en (1995)"), "Se7en ");
    }

    #[test]
    fn clean_title_peels_chained_trailing_suffixes() {
        // A chained tail of format suffixes ("4K UHD BLURAY") must peel off
        // group by group from the END, leaving no suffix fragments behind.
        let out = clean_title("MOVIE_4K_UHD_BLURAY");
        assert!(!out.to_lowercase().contains("uhd"));
        assert!(!out.to_lowercase().contains("bluray"));
        assert_eq!(out, "Movie");
    }

    #[test]
    fn clean_title_empty_input() {
        assert_eq!(clean_title(""), "");
    }

    #[test]
    fn clean_title_multibyte_lowercase_does_not_panic() {
        // 'İ'/'ẞ' change byte length under to_lowercase, so slicing the
        // original at an offset found in the lowercased string used to
        // panic ("not a char boundary"); disc labels are disc-controlled.
        let _ = clean_title("İẞẞdvd");
        let _ = clean_title("İstanbul DVD");
        let _ = clean_title("Straße ẞ Blu-ray");
        // A pure-multibyte label with a trailing suffix still produces output
        // without panicking.
        assert!(!clean_title("İẞẞ 4K UHD").is_empty());
    }

    #[test]
    fn clean_title_keeps_embedded_format_words() {
        // A format word that is NOT at the end must not truncate the title.
        assert_eq!(
            clean_title("DOCUMENTARY_ABOUT_DVD_COLLECTIONS"),
            "Documentary About Dvd Collections"
        );
        assert_eq!(
            clean_title("HOLIDAY_BLURAY_SPECIAL"),
            "Holiday Bluray Special"
        );
    }

    #[test]
    fn clean_title_strips_only_trailing_suffix() {
        // Trailing suffix is still stripped.
        assert_eq!(clean_title("THE_MATRIX_DVD"), "The Matrix");
        // Chained trailing groups peel off one after another.
        assert_eq!(clean_title("THE_MATRIX_4K_UHD_BLURAY"), "The Matrix");
    }

    #[test]
    fn clean_title_strips_suffix_followed_by_trademark_or_punctuation() {
        // Regression: retail labels carry a trademark glyph/punctuation after
        // the format word; whitespace-only trimming left it un-anchored, so
        // it was never stripped. Cleaned title must drop both suffix and junk.
        assert_eq!(clean_title("Fight Club - Ultra HD™"), "Fight Club");
        assert_eq!(clean_title("Wraithline 4K Ultra HD®"), "Wraithline");
        assert_eq!(clean_title("The Matrix Blu-ray."), "The Matrix");
        // Embedded format word still protected even with trailing junk.
        assert_eq!(
            clean_title("DOCUMENTARY_ABOUT_DVD_COLLECTIONS™"),
            "Documentary About Dvd Collections"
        );
    }

    #[test]
    fn urlencoded_keeps_allowed_chars() {
        assert_eq!(urlencoded("hello"), "hello");
        assert_eq!(urlencoded("hello world"), "hello+world");
        assert_eq!(urlencoded("name=value"), "name%3Dvalue");
        assert_eq!(urlencoded("a-b_c.d"), "a-b_c.d");
    }

    #[test]
    fn search_url_encodes_both_key_and_query() {
        // Untrusted disc-label query content cannot break out of the query
        // param or inject extra URL params (SSRF/param-injection guard), and a
        // malformed api_key is encoded rather than corrupting the URL.
        let url = search_multi_url("a&b=c #x", "key with space&evil=1");
        assert!(url.starts_with("https://api.themoviedb.org/3/search/multi?"));
        assert!(!url.contains(' '));
        // Raw '&'/'#'/'=' from inputs must be percent-encoded, never literal
        // separators that would add params or a fragment.
        assert!(url.contains("api_key=key+with+space%26evil%3D1"));
        assert!(url.contains("query=a%26b%3Dc+%23x"));
        // Exactly the two intended params plus page.
        assert_eq!(url.matches('&').count(), 2); // &query= and &page=
        assert!(!url.contains('#'));
    }

    #[test]
    fn norm_keeps_accented_letters() {
        // Accented titles must be able to match exactly (was stripped to ASCII).
        assert_eq!(norm("Amélie"), "amélie");
        assert_eq!(norm("Pokémon"), "pokémon");
        assert_eq!(norm("Amélie"), norm("amélie"));
    }

    // --- pick_best: robust result selection from search/multi ---

    #[test]
    fn pick_best_skips_dateless_collection_ranked_first() {
        // The "Wraithline Part Two" bug: a dateless collection ranks ahead of
        // the 2024 film, so the old results.first() path got year == 0.
        let results = serde_json::json!([
            {"media_type": "collection", "name": "Wraithline Collection", "popularity": 90.0},
            {"media_type": "movie", "title": "Wraithline: Part Two",
             "release_date": "2024-02-27", "popularity": 120.0}
        ]);
        let r = pick_best("", results.as_array().unwrap(), false).expect("must pick the film");
        assert_eq!(r.title, "Wraithline: Part Two");
        assert_eq!(r.year, 2024);
    }

    #[test]
    fn pick_best_prefers_dated_even_at_lower_popularity() {
        // A more popular but dateless movie must lose to the dated one —
        // a year in the library folder matters more than popularity rank.
        let results = serde_json::json!([
            {"media_type": "movie", "title": "Wraithline Part Two",
             "release_date": "", "popularity": 200.0},
            {"media_type": "movie", "title": "Wraithline: Part Two",
             "release_date": "2024-02-27", "popularity": 10.0}
        ]);
        let r = pick_best("", results.as_array().unwrap(), false).unwrap();
        assert_eq!(r.year, 2024);
    }

    #[test]
    fn pick_best_breaks_dated_ties_on_popularity() {
        let results = serde_json::json!([
            {"media_type": "movie", "title": "Low", "release_date": "2010-01-01", "popularity": 5.0},
            {"media_type": "movie", "title": "High", "release_date": "2011-01-01", "popularity": 99.0}
        ]);
        let r = pick_best("", results.as_array().unwrap(), false).unwrap();
        assert_eq!(r.title, "High");
    }

    #[test]
    fn pick_best_prefers_tv_over_a_more_popular_film_namesake_when_flagged() {
        // A season-marked disc ("Longacre Season 5") sets prefer_tv. Both a
        // far more popular FILM and the SERIES match the cleaned title exactly
        // and are dated — the series must win so the disc files as TV.
        let results = serde_json::json!([
            {"media_type": "movie", "title": "Longacre",
             "release_date": "2003-01-01", "popularity": 500.0},
            {"media_type": "tv", "name": "Longacre",
             "first_air_date": "2012-01-08", "popularity": 30.0}
        ]);
        let r = pick_best("Longacre", results.as_array().unwrap(), true).unwrap();
        assert_eq!(r.media_type, "tv");
        assert_eq!(r.year, 2012);
        // Without the flag, popularity wins (the film) — proving the flag, not
        // some incidental ordering, is what selects the series.
        let r2 = pick_best("Longacre", results.as_array().unwrap(), false).unwrap();
        assert_eq!(r2.media_type, "movie");
    }

    #[test]
    fn pick_best_prefer_tv_does_not_override_an_exact_film_over_a_fuzzy_series() {
        // prefer_tv is only a tie-break BELOW exactness: an exact-dated FILM
        // still beats a non-exact (undated) series, so a stray season marker on
        // a film disc can't drag it to an unrelated show.
        let results = serde_json::json!([
            {"media_type": "movie", "title": "Heat",
             "release_date": "1995-12-15", "popularity": 40.0},
            {"media_type": "tv", "name": "Heat", "first_air_date": "", "popularity": 90.0}
        ]);
        let r = pick_best("Heat", results.as_array().unwrap(), true).unwrap();
        assert_eq!(r.media_type, "movie");
        assert_eq!(r.year, 1995);
    }

    #[test]
    fn pick_best_skips_person_results() {
        let results = serde_json::json!([
            {"media_type": "person", "name": "Denis Villeneuve", "popularity": 80.0},
            {"media_type": "movie", "title": "Arrival", "release_date": "2016-11-11", "popularity": 40.0}
        ]);
        let r = pick_best("", results.as_array().unwrap(), false).unwrap();
        assert_eq!(r.title, "Arrival");
    }

    #[test]
    fn pick_best_none_when_no_movie_or_tv() {
        let results = serde_json::json!([
            {"media_type": "person", "name": "Someone", "popularity": 80.0},
            {"media_type": "collection", "name": "Some Collection", "popularity": 50.0}
        ]);
        assert!(pick_best("", results.as_array().unwrap(), false).is_none());
    }

    #[test]
    fn pick_best_tv_uses_name_and_first_air_date() {
        let results = serde_json::json!([
            {"media_type": "tv", "name": "Severance", "first_air_date": "2022-02-18", "popularity": 60.0}
        ]);
        let r = pick_best("", results.as_array().unwrap(), false).unwrap();
        assert_eq!(r.title, "Severance");
        assert_eq!(r.year, 2022);
        assert_eq!(r.media_type, "tv");
    }

    #[test]
    fn pick_best_exact_title_beats_more_popular() {
        // The "Undertow" disc (2024 standalone film) must NOT match the far
        // more popular "Captain Nova: Undertow" (2016): exact title beats popularity.
        let results = serde_json::json!([
            {"media_type": "movie", "title": "Captain Nova: Undertow",
             "release_date": "2016-04-27", "popularity": 200.0},
            {"media_type": "movie", "title": "Undertow",
             "release_date": "2024-04-10", "popularity": 30.0}
        ]);
        let r = pick_best("Undertow", results.as_array().unwrap(), false).unwrap();
        assert_eq!(r.title, "Undertow");
        assert_eq!(r.year, 2024);
    }

    #[test]
    fn pick_best_exact_match_ignores_punctuation_and_case() {
        // Disc label "SKYBURNER ACE" (cleaned) must match "Skyburner: Ace"
        // exactly (punctuation/case-insensitive), beating a more popular near-name.
        let results = serde_json::json!([
            {"media_type": "movie", "title": "Skyburner",
             "release_date": "1986-05-16", "popularity": 90.0},
            {"media_type": "movie", "title": "Skyburner: Ace",
             "release_date": "2022-05-24", "popularity": 50.0}
        ]);
        let r = pick_best("Skyburner Ace", results.as_array().unwrap(), false).unwrap();
        assert_eq!(r.title, "Skyburner: Ace");
        assert_eq!(r.year, 2022);
    }

    // Reverse of prior tests: the exact+dated match arrives FIRST, then a
    // more popular non-exact/dateless candidate arrives SECOND. The correct
    // one must still win — never displaced by a later, merely-more-popular one.
    #[test]
    fn pick_best_exact_dated_first_survives_a_more_popular_non_exact_later() {
        let results = serde_json::json!([
            {"media_type": "movie", "title": "Undertow",
             "release_date": "2024-04-10", "popularity": 30.0},
            {"media_type": "movie", "title": "Captain Nova: Undertow",
             "release_date": "2016-04-27", "popularity": 200.0}
        ]);
        let r = pick_best("Undertow", results.as_array().unwrap(), false).unwrap();
        assert_eq!(
            r.title, "Undertow",
            "an already-exact, dated best must not be displaced by a later, \
             merely more popular non-exact candidate"
        );
        assert_eq!(r.year, 2024);
    }

    /// Same "wrong order" shape for the dated-vs-undated tie-break (not the
    /// exact-match tier): a DATED best found first must survive a later,
    /// more popular but UNDATED candidate.
    #[test]
    fn pick_best_dated_first_survives_a_more_popular_undated_later() {
        let results = serde_json::json!([
            {"media_type": "movie", "title": "Wraithline: Part Two",
             "release_date": "2024-02-27", "popularity": 10.0},
            {"media_type": "movie", "title": "Wraithline Part Two",
             "release_date": "", "popularity": 500.0}
        ]);
        let r = pick_best("", results.as_array().unwrap(), false).unwrap();
        assert_eq!(
            r.year, 2024,
            "a dated best found first must not be displaced by a later, \
             more popular but undated candidate"
        );
    }

    // Verify the error_kind string produced for transport/status errors in
    // fetch_multi never contains the api_key (which lives in the URL query
    // string); replicates fetch_multi's summary logic so a future edit is caught.
    #[test]
    fn tmdb_agent_follows_no_redirects() {
        // The request URL carries the api_key in its query string, so
        // following a 3xx (TMDB compromise, misconfig, or on-path tampering)
        // would hand that key to whatever host the redirect names.
        assert_eq!(
            AGENT.config().max_redirects(),
            0,
            "the TMDB agent must not follow redirects — the request URL \
             carries the api_key"
        );
    }

    #[test]
    fn fetch_multi_error_summary_no_api_key_leak() {
        // Verify the Status variant: just a code, no URL.
        // We can't construct a live ureq::Error::Status without a server, but
        // we can assert the format! template that fetch_multi emits.
        let api_key = "my_secret_api_key";
        let url = search_multi_url("some query", api_key);
        // The URL must contain the key (it's in the query string) — that's the
        // leak risk this test guards against.
        assert!(
            url.contains(api_key) || url.contains("my_secret_api_key"),
            "precondition: api_key must be in the URL"
        );

        // The Status arm produces "HTTP {code}" with no URL in it.
        let status_summary = format!("HTTP {}", 429u16);
        assert!(
            !status_summary.contains(api_key),
            "api_key leaked in status summary: {status_summary}"
        );
        assert!(
            !status_summary.contains("themoviedb.org"),
            "URL leaked in status summary: {status_summary}"
        );

        // A representative transport kind string also must not contain the key.
        let transport_summary = "connection failed";
        assert!(
            !transport_summary.contains(api_key),
            "api_key leaked in transport summary"
        );
    }

    // --- read_capped_bytes: the DoS-cap boundary itself ----------------------

    #[test]
    fn read_capped_bytes_accepts_exactly_at_cap() {
        let body = vec![b'x'; 100];
        let got = read_capped_bytes(std::io::Cursor::new(&body), 100).unwrap();
        assert_eq!(got.len(), 100);
    }

    #[test]
    fn read_capped_bytes_rejects_one_byte_over_cap() {
        let body = vec![b'x'; 101];
        let err = read_capped_bytes(std::io::Cursor::new(&body), 100).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn read_capped_bytes_accepts_well_under_cap() {
        let body = b"short body".to_vec();
        let got = read_capped_bytes(std::io::Cursor::new(&body), MAX_TMDB_BYTES).unwrap();
        assert_eq!(got, body);
    }

    #[test]
    fn read_capped_json_end_to_end_rejects_oversized_body_via_real_response() {
        // Exercise the actual `read_capped_json` against a real
        // `ureq::Response` from a local TCP listener streaming a body over
        // `MAX_TMDB_BYTES` — proving the real function enforces the cap.
        use std::io::Write;
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let oversized_len = MAX_TMDB_BYTES + 1024;

        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = std::io::Read::read(&mut stream, &mut buf); // drain the request
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {oversized_len}\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(header.as_bytes()).unwrap();
            stream
                .write_all(&vec![b'{'; oversized_len as usize])
                .unwrap();
        });

        let resp = ureq::get(&format!("http://{addr}/"))
            .config()
            .timeout_global(Some(std::time::Duration::from_secs(5)))
            .build()
            .call()
            .expect("local server must respond");
        let err = read_capped_json(resp).expect_err("oversized body must be rejected");
        // Check the SPECIFIC message, not just io::ErrorKind: a regressed
        // `take(cap)` would hand serde_json a truncated body, which is ALSO
        // invalid JSON with the SAME ErrorKind — only the message differs.
        assert_eq!(
            err.to_string(),
            "tmdb response exceeded size cap",
            "must be rejected by the SIZE CAP specifically, not a downstream JSON parse failure \
             on a silently-truncated body — got {err}"
        );
        handle.join().unwrap();
    }

    // --- norm(): pin the actual character content, not just cross-equality ---

    #[test]
    fn norm_collapses_separator_runs_to_single_space() {
        // Other norm() tests compare both sides of an equality with
        // parallel inputs, so a regressed collapse-to-space step could
        // degrade both sides identically and still compare equal.
        assert_eq!(norm("Top  Gun"), "top gun");
        assert_eq!(norm("Top Gun: Maverick"), "top gun maverick");
        assert_eq!(norm("Top___Gun"), "top gun");
    }

    // --- search(): the manual "needs review" correction picker ---------------
    // No existing test called `search()` before — every mutation inside it,
    // including replacing the whole body with an empty Vec, passed trivially.

    #[test]
    fn search_empty_api_key_or_query_yields_empty() {
        assert!(search("Some Movie", "", 5).is_empty());
        assert!(search("", "key", 5).is_empty());
        assert!(search("   ", "key", 5).is_empty());
    }

    #[test]
    fn rank_search_results_orders_exact_dated_first_then_dated_then_popularity() {
        // The pure ranking logic `search()` calls after `fetch_multi`, driven
        // directly to avoid a network round trip. Checks the FULL returned
        // order, including a highly-popular but DATELESS decoy sorting last.
        let results = serde_json::json!([
            {"media_type": "movie", "title": "Captain Nova: Undertow",
             "release_date": "2016-04-27", "popularity": 200.0},
            {"media_type": "movie", "title": "Undertow",
             "release_date": "2024-04-10", "popularity": 30.0},
            {"media_type": "movie", "title": "Some Undated Undertow Thing",
             "release_date": "", "popularity": 500.0}
        ]);
        let ranked = rank_search_results("Undertow", results.as_array().unwrap(), 10);
        let titles: Vec<&str> = ranked.iter().map(|r| r.title.as_str()).collect();
        assert_eq!(
            titles,
            vec![
                "Undertow",                    // exact + dated: wins outright
                "Captain Nova: Undertow",      // dated, non-exact
                "Some Undated Undertow Thing", // undated, even though most popular
            ]
        );
    }

    #[test]
    fn rank_search_results_respects_limit() {
        let results = serde_json::json!([
            {"media_type": "movie", "title": "A", "release_date": "2001-01-01", "popularity": 1.0},
            {"media_type": "movie", "title": "B", "release_date": "2002-01-01", "popularity": 2.0},
            {"media_type": "movie", "title": "C", "release_date": "2003-01-01", "popularity": 3.0}
        ]);
        let ranked = rank_search_results("", results.as_array().unwrap(), 2);
        assert_eq!(ranked.len(), 2, "must cap at the requested limit");
    }

    #[test]
    fn rank_search_results_empty_input_yields_empty() {
        assert!(rank_search_results("anything", &[], 5).is_empty());
    }

    // --- lookup(): the guard logic ahead of the untestable network call ------
    // `lookup()` has zero coverage otherwise (happy path needs HTTP mocking,
    // out of scope); the guards deciding whether it attempts a request are pure.

    #[test]
    fn lookup_empty_api_key_returns_none_without_network() {
        // No api_key configured: must short-circuit before ever building a
        // request (an empty key would otherwise round-trip to TMDB and get
        // a 401 on every single insert).
        assert!(lookup("Some Movie", "").is_none());
    }

    #[test]
    fn lookup_blank_query_returns_none_without_network() {
        // A separator-only volume label reduces to an empty query after
        // clean_title; must short-circuit rather than firing a bare
        // `query=&...` request that TMDB answers with HTTP 422.
        assert!(lookup("", "some_api_key").is_none());
        assert!(lookup("   ", "some_api_key").is_none());
    }

    // --- TV season-marker stripping ------------------------------------------

    #[test]
    fn clean_title_strips_trailing_tv_season_markers() {
        // A series disc must resolve to the base show title.
        assert_eq!(clean_title("LONGACRE_SEASON_5_DISC_2"), "Longacre");
        assert_eq!(clean_title("VICTORIA SERIES 2 DISC 2"), "Victoria");
        assert_eq!(clean_title("Turn - Staffel 3 - Disc 2"), "Turn");
        assert_eq!(clean_title("Les Revenants Saison 1"), "Les Revenants");
    }

    #[test]
    fn clean_title_keeps_volume_and_part_which_are_real_film_titles() {
        // "Vol."/"Volume"/"Part" are NOT season markers — they belong to real
        // movie titles and must never be peeled.
        assert_eq!(clean_title("KILL_BILL_VOL_2"), "Kill Bill Vol 2");
        assert_eq!(
            clean_title("GUARDIANS_OF_THE_GALAXY_VOL_2"),
            "Guardians Of The Galaxy Vol 2"
        );
        assert_eq!(clean_title("WRAITHLINE_PART_TWO"), "Wraithline Part Two");
    }

    #[test]
    fn clean_title_keeps_bare_season_word_without_a_number() {
        // "Season" as an actual title word (no trailing number) is preserved.
        assert_eq!(clean_title("SILLY_SEASON"), "Silly Season");
        assert_eq!(clean_title("OPEN_SEASON"), "Open Season");
    }

    #[test]
    fn season_and_disc_from_label() {
        assert_eq!(season_from_label("ENDEAVOUR SEASON 5 DISC 2"), Some(5));
        assert_eq!(season_from_label("VICTORIA SERIES 2 DISC 2"), Some(2));
        assert_eq!(season_from_label("GAMEOFTHRONES_S3_DISC1"), Some(3));
        assert_eq!(season_from_label("Turn - Staffel 3 - Disc 2"), Some(3));
        assert_eq!(season_from_label("THE MATRIX"), None);
        assert_eq!(disc_from_label("ENDEAVOUR SEASON 5 DISC 2"), Some(2));
        assert_eq!(disc_from_label("GOT_S3_D4"), Some(4));
        assert_eq!(disc_from_label("BATMAN_BD1"), Some(1));
        assert_eq!(disc_from_label("THE MATRIX"), None);
    }

    #[test]
    fn parse_episodes_reads_number_name_runtime() {
        let json = serde_json::json!({
            "episodes": [
                {"episode_number": 1, "name": "Muse", "runtime": 89},
                {"episode_number": 2, "name": "Cartouche", "runtime": 89},
                {"episode_number": 3, "name": "Passenger"} // runtime absent -> 0
            ]
        });
        let eps = parse_episodes(&json);
        assert_eq!(eps.len(), 3);
        assert_eq!(
            eps[0],
            Episode {
                number: 1,
                name: "Muse".into(),
                runtime_min: 89
            }
        );
        assert_eq!(eps[2].runtime_min, 0);
    }

    #[test]
    fn parse_episodes_empty_on_missing_array() {
        assert!(parse_episodes(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn map_episodes_orders_from_start_and_names_when_runtime_matches() {
        let eps = vec![
            Episode {
                number: 1,
                name: "Muse".into(),
                runtime_min: 89,
            },
            Episode {
                number: 2,
                name: "Cartouche".into(),
                runtime_min: 89,
            },
        ];
        // Two ripped titles ~89 min, season 5 disc 1 → start at E01.
        let got = map_episodes(&[89.0 * 60.0, 88.0 * 60.0], &eps, 1);
        assert_eq!(
            got,
            vec![
                EpisodeAssignment {
                    episode: 1,
                    name: "Muse".into()
                },
                EpisodeAssignment {
                    episode: 2,
                    name: "Cartouche".into()
                },
            ]
        );
    }

    #[test]
    fn map_episodes_numbers_but_drops_name_on_runtime_mismatch() {
        // A "play all" style 180-min title paired with a 45-min episode: keep the
        // sequential number but refuse the (wrong) name.
        let eps = vec![Episode {
            number: 3,
            name: "Real Ep".into(),
            runtime_min: 45,
        }];
        let got = map_episodes(&[180.0 * 60.0], &eps, 3);
        assert_eq!(
            got,
            vec![EpisodeAssignment {
                episode: 3,
                name: String::new()
            }]
        );
    }

    #[test]
    fn map_episodes_degrades_to_sequential_without_tmdb_data() {
        let got = map_episodes(&[1400.0, 1400.0, 1400.0], &[], 7);
        assert_eq!(
            got.iter().map(|a| a.episode).collect::<Vec<_>>(),
            vec![7, 8, 9]
        );
        assert!(got.iter().all(|a| a.name.is_empty()));
    }

    #[test]
    fn runtime_plausible_tolerates_broadcast_drift_but_rejects_gross() {
        assert!(runtime_plausible(89.0 * 60.0, 89)); // exact
        assert!(runtime_plausible(46.0 * 60.0, 45)); // within tolerance
        assert!(runtime_plausible(60.0 * 60.0, 0)); // unknown ep runtime never rejects
        assert!(!runtime_plausible(180.0 * 60.0, 45)); // play-all vs episode
    }

    // A season whose episodes each run `mins[i]` minutes, numbered from 1.
    fn season(mins: &[u16]) -> Vec<Episode> {
        mins.iter()
            .enumerate()
            .map(|(i, &m)| Episode {
                number: (i + 1) as u16,
                name: format!("E{:02}", i + 1),
                runtime_min: m,
            })
            .collect()
    }

    #[test]
    fn align_repairs_uneven_split_via_distinctive_finale() {
        // 10-ep season, 90-min finale (E10), disc 2 holds E07-10. Uniform-split
        // guess is (2-1)*4+1 = 5, WRONG — alignment must pin it to 7 via the finale.
        let eps = season(&[45, 45, 45, 45, 45, 45, 45, 45, 45, 90]);
        let disc2 = [45.0 * 60.0, 45.0 * 60.0, 45.0 * 60.0, 90.0 * 60.0];
        assert_eq!(align_disc_offset(&disc2, &eps, 5), 7);
    }

    #[test]
    fn align_falls_back_when_runtimes_are_uniform() {
        // No distinguishing signal: every episode ~45 min. Every offset fits
        // equally, so the tie must resolve to the caller's fallback (which is the
        // correct answer for a genuinely uniform-split season anyway).
        let eps = season(&[45, 45, 45, 45, 45, 45, 45, 45, 45, 45]);
        let disc2 = [45.0 * 60.0, 45.0 * 60.0, 45.0 * 60.0, 45.0 * 60.0];
        assert_eq!(align_disc_offset(&disc2, &eps, 5), 5);
    }

    #[test]
    fn align_returns_fallback_without_tmdb_data() {
        // No episode list at all → nothing to align against → fallback verbatim.
        assert_eq!(align_disc_offset(&[2700.0, 2700.0], &[], 5), 5);
        // Episodes present but all runtimes unknown (0) → no signal → fallback.
        let eps = season(&[0, 0, 0, 0, 0, 0]);
        assert_eq!(align_disc_offset(&[2700.0, 2700.0], &eps, 3), 3);
    }

    #[test]
    fn align_pins_first_disc_from_a_distinctive_pilot() {
        // Feature-length pilot (E01, 75 min), the rest 45. Disc 1's fallback is 1
        // and alignment agrees; a stray guess of 3 would still be corrected to 1.
        let eps = season(&[75, 45, 45, 45, 45, 45]);
        let disc1 = [75.0 * 60.0, 45.0 * 60.0, 45.0 * 60.0];
        assert_eq!(align_disc_offset(&disc1, &eps, 1), 1);
        assert_eq!(align_disc_offset(&disc1, &eps, 3), 1);
    }

    #[test]
    fn align_returns_fallback_when_disc_cannot_fit_the_season() {
        // A 4-title disc against a 3-episode season can't align honestly.
        let eps = season(&[45, 45, 45]);
        let disc = [2700.0, 2700.0, 2700.0, 2700.0];
        assert_eq!(align_disc_offset(&disc, &eps, 1), 1);
    }

    #[test]
    fn align_tie_breaks_to_the_fallback_not_the_lowest_number() {
        // Two equally-good positions for a distinctive pair (a 45/60 shape that
        // repeats): the one nearest the fallback must win, so a disc-2 guess is
        // not yanked back to the season's start.
        let eps = season(&[45, 60, 45, 60, 45, 60]);
        let disc = [45.0 * 60.0, 60.0 * 60.0];
        // Fallback 3 sits on the [45,60] at E03/E04 — keep it there.
        assert_eq!(align_disc_offset(&disc, &eps, 3), 3);
        // Fallback 1 sits on E01/E02 — keep it there.
        assert_eq!(align_disc_offset(&disc, &eps, 1), 1);
    }

    #[test]
    fn strip_trailing_season_unit() {
        assert_eq!(
            strip_trailing_season("endeavour season 5"),
            Some("endeavour")
        );
        assert_eq!(strip_trailing_season("victoria series 2"), Some("victoria"));
        assert_eq!(strip_trailing_season("open season"), None); // no number
        assert_eq!(strip_trailing_season("kill bill vol 2"), None); // vol not a marker
        assert_eq!(strip_trailing_season("blade runner 2049"), None); // no marker word
    }

    // --- progressive fallback: query_variants + is_trailing_junk -------------

    #[test]
    fn trailing_junk_peels_edition_region_format_and_codes() {
        for j in [
            "UE", "SE", "Ultimate", "Edition", "Cut", "UK", "NA", "BD", "UHD",
        ] {
            assert!(is_trailing_junk(j), "{j} should be peelable junk");
        }
        for code in ["UPT1", "G51", "BD3", "3D", "4K", "D2"] {
            assert!(is_trailing_junk(code), "{code} (alnum code) should be junk");
        }
    }

    #[test]
    fn trailing_junk_never_peels_sequel_markers() {
        // Pure numbers and roman numerals are sequel markers, not junk —
        // peeling them would resolve a sequel to the original film.
        for keep in ["3", "2049", "II", "III", "IV", "X", "1917"] {
            assert!(
                !is_trailing_junk(keep),
                "{keep} is a sequel/title marker and must be kept"
            );
        }
    }

    #[test]
    fn query_variants_peels_the_ue_that_zeroes_out_tmdb() {
        // The live bug: "Batman v Superman: Dawn of Justice: UE" returns ZERO
        // TMDB hits until the trailing "UE" is peeled. The variant list must
        // include the clean full label first, then the peeled title.
        let v = query_variants("Batman v Superman: Dawn of Justice: UE");
        assert_eq!(v[0], clean_title("Batman v Superman: Dawn of Justice: UE"));
        assert!(
            v.iter()
                .any(|q| norm(q) == norm("Batman v Superman Dawn of Justice")),
            "must produce the UE-stripped title as a fallback variant: {v:?}"
        );
    }

    #[test]
    fn query_variants_stops_at_a_sequel_number() {
        // "ALIEN 3" must NOT fan out to "ALIEN": the trailing 3 is meaningful.
        let v = query_variants("ALIEN_3");
        assert_eq!(v, vec!["Alien 3".to_string()]);
        // Same for a roman-numeral sequel.
        let v = query_variants("ROCKY_II");
        assert_eq!(v, vec!["Rocky Ii".to_string()]);
    }

    #[test]
    fn query_variants_peels_multiple_trailing_codes() {
        // Chained trailing codes peel one at a time, most-specific first.
        let v = query_variants("SPRITELINGS_UPT1");
        assert!(v.contains(&"Spritelings".to_string()), "{v:?}");
        let v = query_variants("NIGHTLINER_3D");
        assert!(v.contains(&"Nightliner".to_string()), "{v:?}");
    }

    #[test]
    fn is_confident_match_agrees_with_a_fallback_resolved_title() {
        // The gate must accept a title that `lookup` could only reach via a
        // peeled variant — otherwise every edition disc parks in review even
        // though the lookup found it. Uses the RAW label (not pre-cleaned).
        assert!(
            is_confident_match(
                "Batman v Superman: Dawn of Justice: UE",
                "Batman v Superman: Dawn of Justice",
                2016
            ),
            "a UE-suffixed label must confidently match the un-suffixed film"
        );
    }

    #[test]
    fn is_confident_match_still_requires_a_year() {
        assert!(!is_confident_match("Some Film UE", "Some Film", 0));
    }

    #[test]
    fn is_confident_match_does_not_accept_a_peeled_sequel_collision() {
        // "ALIEN 3" must NOT be confidently matched to "Alien" (1979): the 3 is
        // never peeled, so no variant equals "Alien".
        assert!(
            !is_confident_match("ALIEN_3", "Alien", 1979),
            "a sequel label must never confidently resolve to the original film"
        );
    }
}
