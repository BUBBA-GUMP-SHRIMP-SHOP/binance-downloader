# Binance downloader

Download Binance monthly spot kline archives at the 1-minute interval from a
text file containing one full `https://data.binance.vision/...zip` URL per line.
Blank lines and lines starting with `#` are ignored. Duplicate URLs are skipped.

```bash
cargo run --release -- data/data.txt --jobs 20
cat data/data.txt | cargo run --release -- --jobs 20
```

Omit the filename to read URLs from stdin (a pipe). An explicit `-` also reads
stdin.

`--jobs` / `-j` controls concurrency (default 20, range 1–256). On a terminal,
stderr displays active downloads with byte progress bars, totals, transfer
speeds, and aggregate counts. The display limits visible rows to fit the terminal;
all active downloads continue running. `--no-progress` or redirected stderr
produces plain completion lines. `COLUMNS` and `LINES` can override display size.

Files are stored beneath
`$ARCHIVE_BASE_PATH/binance-archive/data/spot/monthly/klines/` when that variable
is set, otherwise beneath the operating system's user data directory followed
by the same `binance-archive/...` tree.

Each archive is requested once, streamed to a `.zip.part` file, and renamed after
the complete response is written. HTTP errors never become archive files.
Nonempty existing files are skipped; their contents are not revalidated. Empty
files are downloaded again. Missing remote archives (404) are counted separately.
Other failures and invalid URLs produce a nonzero exit status after remaining
work finishes.

Interrupted processes can leave `.zip.part` files. Remove the relevant stale
file before retrying, after ensuring no other downloader is using it. Exclusive
creation of partial files prevents simultaneous writers from corrupting a file.
Downloads restart from the beginning; byte-range resume is not implemented.

For throughput, use a release build (`cargo build --release`) and tune `--jobs`
to your connection and disk. Raising concurrency indefinitely can reduce throughput
when the server or your connection is saturated. Downloads use 256 KiB write
buffers (about 5 MiB total at the default 20 jobs), a shared connection pool, and
adaptive HTTP/2 flow control when negotiated. Progress callbacks are limited to
10 per second per transfer, plus initial and final updates. Completed downloads
still flush and sync their contents before the final rename.
