//! Stream locally archived monthly spot klines through a Tokio channel.

use anyhow::{Context, Result, ensure};
use std::{
    fs::File,
    io::{BufRead, BufReader},
    path::PathBuf,
};
pub use time::PrimitiveDateTime;
use tokio::sync::mpsc::UnboundedSender;

/// Parse a minute-precision date (`YYYY-MM-DDTHH:MM`), interpreted as UTC.
pub fn parse_date(value: &str) -> Result<PrimitiveDateTime> {
    ensure!(value.len() == 16, "expected YYYY-MM-DDTHH:MM");
    let format =
        time::format_description::parse_borrowed::<3>("[year]-[month]-[day]T[hour]:[minute]")?;
    PrimitiveDateTime::parse(value, &format).context("invalid date")
}

/// Publish CSV rows with opening timestamps in the inclusive UTC date range.
/// Monthly ZIPs are processed chronologically, preserving their CSV row order.
/// Missing months are skipped. Files are decompressed on a blocking worker.
/// Returns the number of published rows; dropping the receiver stops reading.
/// The caller must drop other sender clones for the receiver to reach EOF.
pub async fn publish_lines(
    archive_root: PathBuf,
    symbol: String,
    start: PrimitiveDateTime,
    end: PrimitiveDateTime,
    sender: UnboundedSender<String>,
) -> Result<u64> {
    ensure!(start <= end, "start date must not follow end date");
    ensure!(
        !symbol.is_empty()
            && symbol
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_'),
        "invalid symbol"
    );
    tokio::task::spawn_blocking(move || {
        let start_us = start.assume_utc().unix_timestamp_nanos() / 1_000;
        let end_us = end.assume_utc().unix_timestamp_nanos() / 1_000;
        let mut month = (start.year(), start.month() as u8);
        let last = (end.year(), end.month() as u8);
        let mut published = 0;
        while month <= last {
            if sender.is_closed() {
                break;
            }
            let (year, number) = month;
            let filename = format!("{symbol}-1m-{year:04}-{number:02}.zip");
            let path = archive::monthly_kline_path(&archive_root, &symbol, &filename);
            let file = match File::open(&path) {
                Ok(file) => Some(file),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => {
                    return Err(error).with_context(|| format!("opening {}", path.display()));
                }
            };
            if let Some(file) = file {
                let mut zip = zip::ZipArchive::new(file)
                    .with_context(|| format!("reading {}", path.display()))?;
                let csv_name = filename.replace(".zip", ".csv");
                let csv = zip
                    .by_name(&csv_name)
                    .with_context(|| format!("missing {csv_name} in {}", path.display()))?;
                for (index, line) in BufReader::new(csv).lines().enumerate() {
                    if sender.is_closed() {
                        return Ok(published);
                    }
                    let line =
                        line.with_context(|| format!("reading {csv_name} line {}", index + 1))?;
                    let timestamp: i128 = line
                        .split(',')
                        .next()
                        .unwrap_or("")
                        .parse()
                        .with_context(|| {
                            format!("invalid opening timestamp in {csv_name} line {}", index + 1)
                        })?;
                    // Binance spot archives use microseconds from January 2025.
                    let timestamp_us = if year >= 2025 {
                        timestamp
                    } else {
                        timestamp.checked_mul(1_000).context("timestamp overflow")?
                    };
                    if timestamp_us >= start_us && timestamp_us <= end_us {
                        if sender.send(line).is_err() {
                            return Ok(published);
                        }
                        published += 1;
                    }
                }
            }
            month = if number == 12 {
                (year + 1, 1)
            } else {
                (year, number + 1)
            };
        }
        Ok(published)
    })
    .await
    .context("archive reader task failed")?
}
