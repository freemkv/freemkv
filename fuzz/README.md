# Fuzzing

This cargo-fuzz workspace exercises untrusted inputs without device or network I/O.
Run on an EC2 worker with the repository's sibling path patches configured:

```sh
cargo +nightly fuzz run parsers -- -max_total_time=600 -timeout=10 -rss_limit_mb=4096 -max_len=65536
```

Keep the source SHA, toolchain, corpus hashes, logs and crash artifacts with each
run. Minimize a failure and add a regression test before fixing the product code.
Generated corpus and artifacts remain outside the committed source.
