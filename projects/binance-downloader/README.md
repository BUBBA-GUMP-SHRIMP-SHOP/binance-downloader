# Binance downloader

Download Binance monthly spot kline archives at the 1-minute interval from a
text file containing one full `https://data.binance.vision/...zip` URL per line.
Blank lines and lines starting with `#` are ignored. Duplicate URLs are skipped.
A line containing the exact, case-sensitive phrase `poison pill` ends input
early, including when it appears in a comment. That line and all following lines
are ignored; downloads already started finish normally. This applies to both
files and piped input and does not itself count as an error.

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
produces plain completion lines. `--quiet` / `-q` suppresses progress, completion
lines, and the final summary; errors remain visible. It takes precedence over
`--no-progress`. `COLUMNS` and `LINES` can override display size.

Use `--destination PATH` / `-d PATH` to set the archive root, overriding the
environment and OS defaults. For example:

```bash
cat data/data.txt | cargo run --release -- --destination /mnt/archive --jobs 20
```

This writes to `/mnt/archive/data/spot/monthly/klines/<SYMBOL>/1m/<FILENAME>`.
Relative paths are resolved from the current working directory. Existing-file
checks and temporary downloads use the selected destination as well.

Without this flag, files are stored beneath
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

## Named-pipe Linux service

Install a systemd user service as your normal user (without sudo):

```bash
./scripts/install-service.sh --destination "$HOME/binance-archive" --jobs 20
```

The script builds a release binary, installs a private copy and runner, creates
`$HOME/binance-archive/~incoming-job` with owner-only permissions, and enables
and starts `binance-downloader.service`. The `~` is a literal part of the pipe
name. Without `--destination`, it uses the downloader's normal archive root.
Use `--binary PATH` to install an existing executable, or `--no-start` to install
without enabling or starting the service. Reinstalling stops any active instance
before replacing it; let active downloads finish first.

Send newline-terminated URLs from separate commands or a whole URL list:

```bash
printf '%s\n' 'https://data.binance.vision/data/spot/monthly/klines/BTCUSDT/1m/BTCUSDT-1m-2026-04.zip' > "$HOME/binance-archive/~incoming-job"
cat data/data.txt > "$HOME/binance-archive/~incoming-job"
journalctl --user -u binance-downloader.service -f
```

The service stays running between writers and uses quiet mode. Writes to the
pipe wait if the service is stopped; the pipe is not a persistent job queue.
Keep individual URL lines within the platform's `PIPE_BUF` limit when writing
from multiple producers to prevent interleaving. Duplicate URLs are skipped for
the lifetime of the process; restart the service to retry a previously failed URL.

To finish active work and stop, send a poison pill. It does not automatically
restart, including after a failure:

```bash
printf '%s\n' 'poison pill' > "$HOME/binance-archive/~incoming-job"
systemctl --user start binance-downloader.service  # accept jobs again
systemctl --user disable --now binance-downloader.service  # disable service
```

The service normally runs within your systemd user session. To keep it available
after logout, enable lingering for your account with `loginctl enable-linger "$USER"`
if your system permits it.

## Library usage

The `binance_downloader` library provides a bounded Tokio channel for archive links.
Call `start_downloads` inside a Tokio runtime, then clone the sender for each producer:

```rust,no_run
use binance_downloader::{start_downloads, DownloadOptions};

# async fn example() -> anyhow::Result<()> {
let (sender, downloads) = start_downloads(DownloadOptions::default(), 100)?;
sender.add_link("https://data.binance.vision/data/spot/monthly/klines/BTCUSDT/1m/BTCUSDT-1m-2026-04.zip").await?;
drop(sender);
downloads.await??;
# Ok(())
# }
```

`add_link` validates the URL and waits for queue capacity. It confirms enqueueing;
await the task to observe download failures. All sender clones must be dropped
before the task finishes. Previously queued downloads drain on shutdown. Duplicate
archives are skipped for the lifetime of the worker. `DownloadOptions` controls
archive destination, concurrent downloads, progress display, and quiet mode.
