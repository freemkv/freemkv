# freemkv CLI — local dev helper.
# Mirrors the cross-crate scripts in freemkv-private/scripts/test-all.sh
# but scoped to this single crate.

.PHONY: test build check ci clean

test:
	cargo test --tests

build:
	cargo build --release

check:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings

ci: check build test

clean:
	cargo clean
