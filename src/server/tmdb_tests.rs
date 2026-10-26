use super::*;

#[test]
fn null_media_type_reads_as_movie_in_tmdb_results_and_user_metadata() {
    use crate::server::planner::MediaKind;
    let r: TmdbResult = serde_json::from_str(
        r#"{"title":"T","year":1,"poster_url":"","overview":"","media_type":null}"#,
    )
    .unwrap();
    assert_eq!(r.media_type, MediaKind::Movie);
    let m: crate::server::ripper::staging::UserMetadata = serde_json::from_str(
        r#"{"title":"T","year":1,"media_type":null,"tmdb_id":0,"poster_url":"","overview":""}"#,
    )
    .unwrap();
    assert_eq!(m.media_type, MediaKind::Movie);
    let m: crate::server::ripper::staging::UserMetadata =
        serde_json::from_str(r#"{"title":"T","year":1,"tmdb_id":0,"poster_url":"","overview":""}"#)
            .unwrap();
    assert_eq!(m.media_type, MediaKind::Movie);
}
