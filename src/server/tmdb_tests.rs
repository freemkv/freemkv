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

#[derive(Clone, Default)]
struct LogBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
impl std::io::Write for LogBuf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// A BadUri failure whose own Display prints the rejected URL (and so the api_key in
// its query string) must reach the log only as a fixed label.
#[test]
fn a_transport_failure_never_logs_the_api_key() {
    use tracing_subscriber::layer::SubscriberExt as _;
    let buf = LogBuf::default();
    let sink = buf.clone();
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .with_writer(move || sink.clone())
            .with_ansi(false),
    );
    let got =
        tracing::subscriber::with_default(subscriber, || fetch_url("api_key=SECRETKEY123", "q"));
    assert!(got.is_none());
    let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert!(out.contains("tmdb: request failed"), "logged: {out}");
    assert!(!out.contains("SECRETKEY123"), "key leaked: {out}");
}

#[test]
fn a_body_that_stalls_after_the_headers_times_out() {
    use std::io::Write as _;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut c, _) = listener.accept().unwrap();
        let _ = c.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{");
        std::thread::sleep(std::time::Duration::from_secs(4));
    });
    let agent = build_agent(std::time::Duration::from_millis(300));
    let started = std::time::Instant::now();
    let resp = agent
        .get(format!("http://127.0.0.1:{port}/"))
        .call()
        .unwrap();
    assert!(read_capped_json(resp).is_err());
    assert!(
        started.elapsed() < std::time::Duration::from_secs(3),
        "the body read has its own deadline"
    );
    server.join().unwrap();
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

#[test]
fn urlencoded_percent_encodes_each_utf8_byte() {
    assert_eq!(urlencoded("Amélie"), "Am%C3%A9lie");
    assert_eq!(urlencoded("千"), "%E5%8D%83");
}

#[test]
fn parse_result_carries_poster_overview_id_and_the_right_date() {
    let movie = serde_json::json!({"media_type": "movie", "title": "Heat", "id": 949,
            "release_date": "1995-12-15", "first_air_date": "2001-01-01",
            "poster_path": "/h.jpg", "overview": "A heist.", "popularity": 3.0});
    let (r, pop) = parse_result(&movie).unwrap();
    assert_eq!(
        (r.year, r.tmdb_id, r.overview.as_str()),
        (1995, 949, "A heist.")
    );
    assert_eq!(r.poster_url, "https://image.tmdb.org/t/p/w300/h.jpg");
    assert_eq!(pop, 3.0);
    let tv = serde_json::json!({"media_type": "tv", "name": "Severance", "id": 95396,
            "release_date": "1999-01-01", "first_air_date": "2022-02-18"});
    let (r, _) = parse_result(&tv).unwrap();
    assert_eq!(
        (r.year, r.tmdb_id, r.title.as_str()),
        (2022, 95396, "Severance")
    );
    let slashless = serde_json::json!({"media_type": "movie", "title": "X",
            "poster_path": "h.jpg"});
    assert_eq!(parse_result(&slashless).unwrap().0.poster_url, "");
    let untitled = serde_json::json!({"media_type": "movie", "title": ""});
    assert!(parse_result(&untitled).is_none());
}

#[test]
fn season_and_disc_numbers_are_one_to_ninety_nine() {
    assert_eq!(season_from_label("Show Season 0"), None);
    assert_eq!(season_from_label("Show Season 99"), Some(99));
    assert_eq!(season_from_label("Show Season 100"), None);
    assert_eq!(season_from_label("Show S2019"), None);
    assert_eq!(season_from_label("Show Season 05"), Some(5));
    assert_eq!(season_from_label("Show Season 99999999"), None);
    assert_eq!(disc_from_label("Show Disc 0"), None);
    assert_eq!(disc_from_label("Show D100"), None);
    assert_eq!(disc_from_label("Show D2"), Some(2));
}

#[test]
fn the_runtime_tolerance_is_the_larger_of_five_minutes_and_a_quarter() {
    // 25% of 45 is 11.25: a gap of 8 is fine, 12 is not.
    assert!(runtime_plausible(53.0 * 60.0, 45));
    assert!(!runtime_plausible(57.0 * 60.0, 45));
    // 25% of 10 is 2.5, so the 5-minute floor decides: 4 is fine, 6 is not.
    assert!(runtime_plausible(14.0 * 60.0, 10));
    assert!(!runtime_plausible(16.0 * 60.0, 10));
}

#[test]
fn a_season_word_inside_a_longer_word_is_not_a_season_marker() {
    assert_eq!(strip_trailing_season("offseason 2"), None);
    assert_eq!(strip_trailing_season("postseason 3"), None);
    assert_eq!(strip_trailing_season("off season 2"), Some("off"));
    assert_eq!(strip_trailing_season("season 2"), Some(""));
}

#[test]
fn align_keeps_the_fallback_unless_another_offset_is_clearly_better() {
    // Runtimes differ by under a minute: noise, not a reason to renumber disc 2.
    let eps = season(&[22, 23, 22, 22]);
    let disc2 = [22.8 * 60.0, 22.8 * 60.0];
    assert_eq!(align_disc_offset(&disc2, &eps, 3), 3);
}

fn results_for(title: &str, year: &str, kind: &str) -> serde_json::Value {
    let (name, date) = if kind == "tv" {
        ("name", "first_air_date")
    } else {
        ("title", "release_date")
    };
    serde_json::json!({"results": [{"media_type": kind, name: title, date: year,
            "popularity": 1.0}]})
}

#[test]
fn lookup_returns_the_first_confident_variant_else_the_first_fallback() {
    // "Heat UE" peels to "Heat". The full label finds a loose, undated guess; the
    // peeled variant finds the exact dated film, which must win.
    let found = lookup_with("HEAT_UE", |q| match q {
        "Heat Ue" => Some(results_for("Heat Wave", "", "movie")),
        "Heat" => Some(results_for("Heat", "1995-12-15", "movie")),
        _ => None,
    })
    .unwrap();
    assert_eq!((found.title.as_str(), found.year), ("Heat", 1995));
    // No variant is confident: the first variant's guess is kept, not a later one.
    let guess = lookup_with("HEAT_UE", |q| match q {
        "Heat Ue" => Some(results_for("Heat Wave", "2001-01-01", "movie")),
        "Heat" => Some(results_for("Heated", "2002-02-02", "movie")),
        _ => None,
    })
    .unwrap();
    assert_eq!(guess.title, "Heat Wave");
    // An exact title with no year is not confident either.
    let undated = lookup_with("HEAT", |_| Some(results_for("Heat", "", "movie")));
    assert_eq!(undated.unwrap().year, 0);
}

#[test]
fn lookup_prefers_the_series_for_a_season_marked_label() {
    let both = serde_json::json!({"results": [
            {"media_type": "movie", "title": "Longacre", "release_date": "2003-01-01",
             "popularity": 500.0},
            {"media_type": "tv", "name": "Longacre", "first_air_date": "2012-01-08",
             "popularity": 30.0}]});
    let tv = lookup_with("Longacre Season 5", |_| Some(both.clone())).unwrap();
    assert_eq!(tv.media_type, "tv");
    let film = lookup_with("Longacre", |_| Some(both.clone())).unwrap();
    assert_eq!(film.media_type, "movie");
}
