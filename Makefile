# freemkv — local dev helper.
# Mirrors the workspace-wide CI checks but scoped to this single crate.
# Each target covers both builds: the CLI (no features) and the app (gui).

.PHONY: test build app check ci clean

test:
	cargo test --tests
	cargo test --tests --features gui

build:
	cargo build --release --bin freemkv

app:
	cargo build --release --features gui

check:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
	cargo clippy --all-targets --features gui -- -D warnings

ci: check build app test

clean:
	cargo clean
