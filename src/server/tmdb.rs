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
// connection would wedge the rip thread or a web handler indefinitely. The body gets its own
// bound: headers arriving in time says nothing about a body that then stalls.
static AGENT: once_cell::sync::Lazy<ureq::Agent> =
    once_cell::sync::Lazy::new(|| build_agent(std::time::Duration::from_secs(10)));

fn build_agent(recv_body: std::time::Duration) -> ureq::Agent {
    let config = ureq::config::Config::builder()
        .timeout_connect(Some(std::time::Duration::from_secs(5)))
        .timeout_recv_response(Some(std::time::Duration::from_secs(10)))
        .timeout_recv_body(Some(recv_body))
        // Follow NO redirects: the URL carries the operator's api_key in its
        // query string, so a 3xx (TMDB compromised/tampered on-path) would
        // hand that key to an arbitrary host; `fetch_multi` reports it as "no result".
        .max_redirects(0)
        .build();
    ureq::Agent::new_with_config(config)
}

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
    fetch_url(&search_multi_url(query, api_key), query)
}

fn fetch_url(url: &str, query: &str) -> Option<serde_json::Value> {
    match AGENT.get(url).call() {
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
            let error_kind = crate::server::web::ureq_error_kind(&e);
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
            "tmdb: API key rejected (HTTP 401) — check the TMDB API key in Settings; \
             titles will fall through to the needs-review queue until it is fixed"
        );
        crate::server::log::syslog(
            "TMDB API key rejected (HTTP 401) — check the TMDB API key in Settings",
        );
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
    lookup_with(label, |variant| fetch_multi(variant, api_key))
}

// `lookup`'s variant loop over any `search/multi` source.
fn lookup_with(
    label: &str,
    fetch: impl Fn(&str) -> Option<serde_json::Value>,
) -> Option<TmdbResult> {
    // A separator-only label yields no query variants; short-circuit rather
    // than firing `query=&...` (TMDB answers HTTP 422). A season marker
    // ("… Season 5") means TV — bias the pick so it can't be outranked.
    let prefer_tv = season_from_label(label).is_some();
    let mut fallback: Option<TmdbResult> = None;
    for variant in query_variants(label) {
        if variant.trim().is_empty() {
            continue;
        }
        let Some(resp) = fetch(&variant) else {
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
            let error_kind = crate::server::web::ureq_error_kind(&e);
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

#[cfg(test)]
#[path = "tmdb_tests.rs"]
mod tests;
