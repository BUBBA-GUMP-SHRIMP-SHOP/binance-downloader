use kline_retriever::{parse_date, publish_lines};
use std::{
    fs::{self, File},
    io::Write,
    path::Path,
};
use tokio::sync::mpsc;

fn write_zip(root: &Path, month: &str, body: &str) {
    let name = format!("BTCUSDT-1m-{month}");
    let path = archive::monthly_kline_path(root, "BTCUSDT", &format!("{name}.zip"));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut zip = zip::ZipWriter::new(File::create(path).unwrap());
    zip.start_file(
        format!("{name}.csv"),
        zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated),
    )
    .unwrap();
    zip.write_all(body.as_bytes()).unwrap();
    zip.finish().unwrap();
}

fn row(date: &str, micros: bool) -> String {
    let timestamp = parse_date(date).unwrap().assume_utc().unix_timestamp();
    format!(
        "{},1,2,3,4,5",
        timestamp * if micros { 1_000_000 } else { 1_000 }
    )
}

#[tokio::test(flavor = "current_thread")]
async fn publishes_chronologically_with_inclusive_bounds_and_mixed_timestamp_units() {
    let root = tempfile::tempdir().unwrap();
    let before = row("2024-12-31T23:58", false);
    let first = row("2024-12-31T23:59", false);
    let second = row("2025-01-01T00:00", true);
    let last = row("2025-03-01T00:00", true);
    let after = row("2025-03-01T00:01", true);
    // Create in reverse order, leaving February absent.
    write_zip(root.path(), "2025-03", &format!("{last}\n{after}\n"));
    write_zip(root.path(), "2025-01", &format!("{second}\n"));
    write_zip(root.path(), "2024-12", &format!("{before}\r\n{first}"));
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let publisher = tokio::spawn(publish_lines(
        root.path().into(),
        "BTCUSDT".into(),
        parse_date("2024-12-31T23:59").unwrap(),
        parse_date("2025-03-01T00:00").unwrap(),
        sender,
    ));
    let mut lines = Vec::new();
    while let Some(line) = receiver.recv().await {
        lines.push(line);
    }
    assert_eq!(publisher.await.unwrap().unwrap(), 3);
    assert_eq!(lines, [first, second, last]);
}

#[tokio::test]
async fn empty_archive_closes_channel_and_receiver_drop_cancels() {
    let root = tempfile::tempdir().unwrap();
    let date = parse_date("2026-01-01T00:00").unwrap();
    let (sender, mut receiver) = mpsc::unbounded_channel();
    assert_eq!(
        publish_lines(root.path().into(), "BTCUSDT".into(), date, date, sender)
            .await
            .unwrap(),
        0
    );
    assert_eq!(receiver.recv().await, None);
    write_zip(root.path(), "2026-01", "invalid timestamp");
    let (sender, receiver) = mpsc::unbounded_channel();
    drop(receiver);
    assert_eq!(
        publish_lines(root.path().into(), "BTCUSDT".into(), date, date, sender)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn rejects_invalid_ranges_symbols_corrupt_zips_and_rows() {
    let root = tempfile::tempdir().unwrap();
    let start = parse_date("2026-01-01T00:00").unwrap();
    let end = parse_date("2026-02-01T00:00").unwrap();
    for (symbol, from, to) in [("BTCUSDT", end, start), ("../BTCUSDT", start, end)] {
        let (sender, _receiver) = mpsc::unbounded_channel();
        assert!(
            publish_lines(root.path().into(), symbol.into(), from, to, sender)
                .await
                .is_err()
        );
    }
    write_zip(root.path(), "2026-01", "not-a-timestamp,1,2\n");
    let (sender, _receiver) = mpsc::unbounded_channel();
    assert!(
        publish_lines(root.path().into(), "BTCUSDT".into(), start, end, sender)
            .await
            .is_err()
    );
    let path = archive::monthly_kline_path(root.path(), "BTCUSDT", "BTCUSDT-1m-2026-01.zip");
    fs::write(path, "broken zip").unwrap();
    let (sender, _receiver) = mpsc::unbounded_channel();
    assert!(
        publish_lines(root.path().into(), "BTCUSDT".into(), start, end, sender)
            .await
            .is_err()
    );
    for invalid in ["2026-02-30T00:00", "2026-01-01T24:00", "2026-01-01T00:00Z"] {
        assert!(parse_date(invalid).is_err());
    }
}
