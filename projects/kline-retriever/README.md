# kline-retriever

Stream local monthly one-minute spot kline CSV rows from ZIP archives.

```bash
cargo run -p kline-retriever -- BTCUSDT 2026-01-01T00:00 2026-02-01T00:00
```

An optional fourth argument sets the archive root. Otherwise the shared `archive`
crate resolves it from `ARCHIVE_BASE_PATH` or the OS data directory.

Dates are UTC and inclusive, filtered using each row's opening timestamp.
Missing monthly files are skipped. ZIPs are read in chronological month order,
preserving CSV row order and removing line endings. Timestamp units are milliseconds
before 2025 and microseconds from January 2025. Invalid ZIPs, missing expected CSV
entries, invalid timestamps, and read failures return errors. Only local archives
are read; missing data is not downloaded.

## Library

`publish_lines` runs file reading and decompression on a blocking worker and feeds
a Tokio unbounded channel. Its result is the number of published rows.

```rust,no_run
use kline_retriever::{parse_date, publish_lines};
use tokio::sync::mpsc;

# async fn example() -> anyhow::Result<()> {
let (sender, mut receiver) = mpsc::unbounded_channel();
let publisher = tokio::spawn(publish_lines(
    archive::default_archive_root(),
    "BTCUSDT".into(),
    parse_date("2026-01-01T00:00")?,
    parse_date("2026-02-01T00:00")?,
    sender,
));
while let Some(line) = receiver.recv().await {
    println!("{line}");
}
let published = publisher.await??;
# Ok(())
# }
```

The queue is unbounded, so producers can run ahead of consumers. Dropping the
receiver stops the publisher. The publisher drops its sender on completion;
drop any other sender clones for `recv()` to return `None`. Always await the
publisher task to distinguish errors from successful completion.
