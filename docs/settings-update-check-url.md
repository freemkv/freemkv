# `update_check_url_names_the_repo_releases_are_actually_published_to`

`check_for_update`'s GitHub API URL must name the same `owner/repo`
that releases are actually published to — not some other repo that
happens to share a similar name. Rather than hardcoding the expected
repo path a second time (which would just restate the constant and
prove nothing if both copies were wrong the same way), this pulls the
URL out of this file's own source via `include_str!` — the same
self-inspection pattern `mac.rs`/`windows.rs`/`engine.rs`/`pipe.rs`
use — and cross-checks the `owner/repo` it names against the release
download links baked into the repo's own README.
