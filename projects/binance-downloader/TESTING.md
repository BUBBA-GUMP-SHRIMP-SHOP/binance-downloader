# Testing

```bash
cargo test --offline
cargo clippy --offline --all-targets -- -D warnings
cargo fmt --check
```

Tests cover strict Binance URL parsing, path construction, bounded concurrency
on a single-thread runtime, zero-worker rejection, and poison-pill termination
with graceful completion of previously started workers. Local HTTP server tests
verify successful streaming, skipping completed files, 404 handling, HTTP error
rejection, partial-file cleanup after a truncated response, and multi-megabyte buffered
writes including a final partial buffer. They require
permission to bind a loopback socket and never contact Binance.

For a manual terminal check, run with a URL list and `--jobs 3`. Confirm that
active rows show byte counts and speeds and that completed downloads leave a
summary. Repeat with `--no-progress` or redirect stderr to verify plain logging.

Service installer syntax can be checked with `bash -n scripts/install-service.sh`.
The installer has also been checked using a mocked systemctl and temporary XDG
folders, systemd unit verification, repeated FIFO writers, poison-pill shutdown,
and refusal to replace a non-FIFO file. No real service was installed for these
checks.
