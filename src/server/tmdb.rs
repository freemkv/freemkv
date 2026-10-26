//! Server diagnostics adapter; all identification and numbering lives in the engine.
pub use freemkv_engine::metadata::tmdb::*;

struct Diagnostics;
impl freemkv_engine::Sink for Diagnostics {
    fn log(&self, _: freemkv_engine::Level, message: &str) {
        super::log::syslog(message);
    }
}
pub fn lookup(label: &str, api_key: &str) -> Result<Option<TmdbResult>, LookupFailed> {
    Client::new(api_key, &Diagnostics).lookup(label)
}
pub fn search(query: &str, api_key: &str, limit: usize) -> Vec<TmdbResult> {
    Client::new(api_key, &Diagnostics).search(query, limit)
}
pub fn season_episodes(tv_id: u64, season: u16, api_key: &str) -> Vec<Episode> {
    Client::new(api_key, &Diagnostics).season_episodes(tv_id, season)
}

#[cfg(test)]
#[path = "tmdb_tests.rs"]
mod tests;
