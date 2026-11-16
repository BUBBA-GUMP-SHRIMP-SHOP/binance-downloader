mod gain;

use kline_retriever::{PrimitiveDateTime, parse_date, publish_lines};
use rocket::serde::{Deserialize, Serialize, json::Json};
use rocket::tokio::{
    self,
    io::{AsyncRead, ReadBuf},
    sync::mpsc,
    task::JoinHandle,
};
use rocket::{Build, Rocket, State, get, launch, routes};
use rocket::{
    http::{ContentType, Status},
    response::{status, stream::ReaderStream},
};
use std::{
    future::Future,
    io,
    path::{Path, PathBuf},
    pin::Pin,
    task::{Context, Poll},
};

struct ArchiveRoot(PathBuf, std::sync::Arc<tokio::sync::Mutex<()>>);

async fn ensure_downloaded(
    root: &Path,
    symbol: &str,
    start: PrimitiveDateTime,
    end: PrimitiveDateTime,
) -> anyhow::Result<()> {
    use binance_downloader::{DownloadOptions, start_downloads};
    let options = DownloadOptions {
        destination: Some(root.to_path_buf()),
        quiet: true,
        ..Default::default()
    };
    let (sender, downloads) = start_downloads(options, 100)?;
    let mut month = (start.year(), start.month() as u8);
    let last = (end.year(), end.month() as u8);
    let queued: anyhow::Result<()> = async {
        while month <= last {
            let (year, number) = month;
            let filename = format!("{symbol}-1m-{year:04}-{number:02}.zip");
            sender
                .add_link(format!(
                    "https://data.binance.vision/data/spot/monthly/klines/{symbol}/1m/{filename}"
                ))
                .await?;
            month = if number == 12 {
                (year + 1, 1)
            } else {
                (year, number + 1)
            };
        }
        Ok(())
    }
    .await;
    drop(sender);
    let completed = downloads.await?;
    queued?;
    completed?;
    Ok(())
}

type ApiError = status::Custom<String>;

fn invalid(message: impl ToString) -> ApiError {
    status::Custom(Status::UnprocessableEntity, message.to_string())
}

fn archive_error(error: impl std::fmt::Display) -> ApiError {
    eprintln!("Archive retrieval failed: {error}");
    status::Custom(
        Status::InternalServerError,
        "Archive retrieval failed".into(),
    )
}

#[derive(Clone, Copy)]
enum OutputFormat {
    Csv,
    Jsonl { carry_forward: bool, gain: bool },
}

#[derive(Clone, Serialize)]
#[serde(crate = "rocket::serde")]
struct KlineJson {
    open_time: u64,
    open: serde_json::Number,
    high: serde_json::Number,
    low: serde_json::Number,
    close: serde_json::Number,
    volume: serde_json::Number,
}

impl KlineJson {
    fn parse(line: &str) -> io::Result<Self> {
        let mut fields = line.split(',');
        let open_time = fields
            .next()
            .unwrap_or("")
            .parse::<u64>()
            .map_err(io::Error::other)?;
        let mut number = || -> io::Result<serde_json::Number> {
            fields
                .next()
                .ok_or_else(|| io::Error::other("missing kline column"))?
                .parse()
                .map_err(io::Error::other)
        };
        Ok(Self {
            open_time,
            open: number()?,
            high: number()?,
            low: number()?,
            close: number()?,
            volume: number()?,
        })
    }

    // Spot archives switched timestamp units at 2025-01-01 UTC.
    const MICROSECOND_START: u64 = 1_735_689_600_000_000;

    fn micros(&self) -> io::Result<u64> {
        if self.open_time >= Self::MICROSECOND_START {
            Ok(self.open_time)
        } else {
            self.open_time
                .checked_mul(1_000)
                .ok_or_else(|| io::Error::other("timestamp overflow"))
        }
    }

    fn encode(&self) -> io::Result<Vec<u8>> {
        let mut bytes = serde_json::to_vec(self).map_err(io::Error::other)?;
        bytes.push(b'\n');
        Ok(bytes)
    }
}

/// Calculate the five gains from later/former, keeping the former timestamp.
fn gain_row(former: &KlineJson, later: &KlineJson) -> io::Result<Vec<u8>> {
    #[derive(Serialize)]
    #[serde(crate = "rocket::serde")]
    struct GainRow {
        open_time: u64,
        open: Option<f64>,
        high: Option<f64>,
        low: Option<f64>,
        close: Option<f64>,
        volume: Option<f64>,
    }
    let calculate = |former: &serde_json::Number, later: &serde_json::Number| {
        gain::gain(former.as_f64()?, later.as_f64()?)
    };
    let row = GainRow {
        open_time: former.open_time,
        open: calculate(&former.open, &later.open),
        high: calculate(&former.high, &later.high),
        low: calculate(&former.low, &later.low),
        close: calculate(&former.close, &later.close),
        volume: calculate(&former.volume, &later.volume),
    };
    let mut bytes = serde_json::to_vec(&row).map_err(io::Error::other)?;
    bytes.push(b'\n');
    Ok(bytes)
}

// Propagate publisher failures as body read errors if streaming has begun.
struct KlineReader {
    format: OutputFormat,
    previous: Option<KlineJson>,
    next: Option<KlineJson>,
    receiver: mpsc::UnboundedReceiver<String>,
    publisher: Option<JoinHandle<anyhow::Result<u64>>>,
    pending: io::Cursor<Vec<u8>>,
}

impl KlineReader {
    fn set_line(&mut self, line: String) -> io::Result<()> {
        match self.format {
            OutputFormat::Csv => self.pending = io::Cursor::new(format!("{line}\n").into_bytes()),
            OutputFormat::Jsonl { .. } => self.set_record(KlineJson::parse(&line)?)?,
        }
        Ok(())
    }

    fn set_record(&mut self, record: KlineJson) -> io::Result<()> {
        let output = if matches!(
            self.format,
            OutputFormat::Jsonl {
                carry_forward: true,
                ..
            }
        ) {
            if let Some(previous) = &self.previous {
                let next_time = previous
                    .micros()?
                    .checked_add(60_000_000)
                    .ok_or_else(|| io::Error::other("timestamp overflow"))?;
                if next_time < record.micros()? {
                    let mut filled = previous.clone();
                    filled.open_time = if next_time >= KlineJson::MICROSECOND_START {
                        next_time
                    } else {
                        next_time / 1_000
                    };
                    self.next = Some(record);
                    filled
                } else {
                    record
                }
            } else {
                record
            }
        } else {
            record
        };
        let bytes = if matches!(self.format, OutputFormat::Jsonl { gain: true, .. }) {
            match &self.previous {
                Some(former) => gain_row(former, &output)?,
                None => Vec::new(),
            }
        } else {
            output.encode()?
        };
        self.pending = io::Cursor::new(bytes);
        self.previous = Some(output);
        Ok(())
    }
}

impl AsyncRead for KlineReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        loop {
            let before = buf.filled().len();
            std::task::ready!(Pin::new(&mut self.pending).poll_read(cx, buf))?;
            if buf.filled().len() > before {
                return Poll::Ready(Ok(()));
            }
            if let Some(next) = self.next.take() {
                self.set_record(next)?;
                continue;
            }
            match self.receiver.poll_recv(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(line)) => {
                    self.set_line(line)?;
                }
                Poll::Ready(None) => {
                    let Some(task) = self.publisher.as_mut() else {
                        return Poll::Ready(Ok(()));
                    };
                    let result = std::task::ready!(Pin::new(task).poll(cx));
                    self.publisher = None;
                    return Poll::Ready(match result {
                        Ok(Ok(_)) => Ok(()),
                        Ok(Err(error)) => Err(io::Error::other(format!("{error:#}"))),
                        Err(error) => Err(io::Error::other(error)),
                    });
                }
            }
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(crate = "rocket::serde", rename_all = "camelCase", deny_unknown_fields)]
struct DateRange {
    start_date: MinuteDateTime,
    #[serde(
        default,
        deserialize_with = "deserialize_optional",
        skip_serializing_if = "Option::is_none"
    )]
    end_date: Option<MinuteDateTime>,
    symbol: String,
}

fn deserialize_optional<'de, T: Deserialize<'de>, D: rocket::serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(deserializer).map(Some)
}

#[derive(Serialize)]
#[serde(crate = "rocket::serde", transparent)]
struct MinuteDateTime(String);

impl<'de> Deserialize<'de> for MinuteDateTime {
    fn deserialize<D: rocket::serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        let bytes = value.as_bytes();
        let valid = bytes.len() == 16
            && bytes.iter().enumerate().all(|(i, byte)| match i {
                4 | 7 => *byte == b'-',
                10 => *byte == b'T',
                13 => *byte == b':',
                _ => byte.is_ascii_digit(),
            })
            && &bytes[11..13] <= b"23".as_slice()
            && &bytes[14..16] <= b"59".as_slice();
        if valid {
            Ok(Self(value))
        } else {
            Err(rocket::serde::de::Error::custom(
                "expected YYYY-MM-DDTHH:MM with hours 00-23 and minutes 00-59",
            ))
        }
    }
}

#[rocket::post("/klines", format = "json", data = "<range>")]
async fn klines(
    range: Json<DateRange>,
    root: &State<ArchiveRoot>,
) -> Result<(ContentType, ReaderStream![KlineReader]), ApiError> {
    let reader = start_stream(range.into_inner(), root, OutputFormat::Csv).await?;
    Ok((ContentType::new("text", "csv"), ReaderStream::one(reader)))
}

#[derive(rocket::FromForm)]
struct JsonlOptions {
    #[field(name = "carry-forward")]
    carry_forward: Option<bool>,
    gain: Option<bool>,
}

#[rocket::post("/klines-jsonl?<options..>", format = "json", data = "<range>")]
async fn klines_jsonl(
    options: JsonlOptions,
    range: Json<DateRange>,
    root: &State<ArchiveRoot>,
) -> Result<(ContentType, ReaderStream![KlineReader]), ApiError> {
    let reader = start_stream(
        range.into_inner(),
        root,
        OutputFormat::Jsonl {
            carry_forward: options.carry_forward.unwrap_or(false),
            gain: options.gain.unwrap_or(false),
        },
    )
    .await?;
    Ok((
        ContentType::new("application", "x-ndjson"),
        ReaderStream::one(reader),
    ))
}

async fn start_stream(
    range: DateRange,
    root: &ArchiveRoot,
    format: OutputFormat,
) -> Result<KlineReader, ApiError> {
    let start = parse_date(&range.start_date.0).map_err(invalid)?;
    let end = match &range.end_date {
        Some(date) => parse_date(&date.0).map_err(invalid)?,
        None => {
            let now = time::OffsetDateTime::now_utc();
            PrimitiveDateTime::new(now.date(), now.time())
        }
    };
    if start > end {
        return Err(invalid("startDate must not follow endDate"));
    }
    if range.symbol.is_empty()
        || !range
            .symbol
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err(invalid("invalid symbol"));
    }
    // Keep the gate until downloads finish even if this request disconnects.
    let download_root = root.0.clone();
    let download_symbol = range.symbol.clone();
    let gate = root.1.clone();
    // Download failures do not prevent serving whatever is available locally.
    let _ = tokio::spawn(async move {
        let _guard = gate.lock().await;
        ensure_downloaded(&download_root, &download_symbol, start, end).await
    })
    .await;
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let publisher = tokio::spawn(publish_lines(
        root.0.clone(),
        range.symbol.clone(),
        start,
        end,
        sender,
    ));
    // Detect failures before the first matching row before sending HTTP headers.
    let first = receiver.recv().await;
    let publisher = if first.is_none() {
        publisher
            .await
            .map_err(archive_error)?
            .map_err(archive_error)?;
        None
    } else {
        Some(publisher)
    };
    let mut reader = KlineReader {
        format,
        previous: None,
        next: None,
        receiver,
        publisher,
        pending: io::Cursor::new(Vec::new()),
    };
    if let Some(line) = first {
        reader.set_line(line).map_err(archive_error)?;
    }
    Ok(reader)
}

#[get("/")]
fn index() -> &'static str {
    "kline-web-provider"
}

#[get("/health")]
fn health() -> &'static str {
    "ok"
}

#[launch]
fn rocket() -> Rocket<Build> {
    build_rocket(archive::default_archive_root())
}

fn build_rocket(root: PathBuf) -> Rocket<Build> {
    rocket::build()
        .manage(ArchiveRoot(
            root,
            std::sync::Arc::new(tokio::sync::Mutex::new(())),
        ))
        .mount("/", routes![index, health, klines, klines_jsonl])
}

#[cfg(test)]
mod tests {
    use super::build_rocket;
    use rocket::{
        http::{ContentType, Status},
        local::blocking::Client,
    };

    fn write_month(root: &std::path::Path, year: i32, month: u8, body: &[u8]) {
        use std::io::Write;
        let name = format!("BTCUSDT-1m-{year:04}-{month:02}");
        let path = archive::monthly_kline_path(root, "BTCUSDT", &format!("{name}.zip"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut zip = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        zip.start_file(
            format!("{name}.csv"),
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        zip.write_all(body).unwrap();
        zip.finish().unwrap();
    }

    fn write_zip(root: &std::path::Path) {
        write_month(
            root,
            2025,
            1,
            b"1735689600000000,1,2,3,4,5\n1735689660000000,6,7,8,9,10\n",
        );
    }

    #[test]
    fn klines_accepts_date_ranges() {
        let root = tempfile::tempdir().unwrap();
        let client = Client::tracked(build_rocket(root.path().into())).unwrap();
        let now = time::OffsetDateTime::now_utc();
        let mut month = (2025, 1);
        while month <= (now.year(), now.month() as u8) {
            write_month(root.path(), month.0, month.1, b"");
            month = if month.1 == 12 {
                (month.0 + 1, 1)
            } else {
                (month.0, month.1 + 1)
            };
        }
        write_zip(root.path());
        for body in [
            r#"{"startDate":"2025-01-01T00:00","endDate":"2025-01-01T00:01","symbol":"BTCUSDT"}"#,
            r#"{"startDate":"2025-01-01T00:00","symbol":"BTCUSDT"}"#,
        ] {
            let response = client
                .post("/klines")
                .header(ContentType::JSON)
                .body(body)
                .dispatch();
            assert_eq!(response.status(), Status::Ok);
            assert_eq!(
                response.content_type(),
                Some(ContentType::new("text", "csv"))
            );
            assert_eq!(
                response.into_string().unwrap(),
                "1735689600000000,1,2,3,4,5\n1735689660000000,6,7,8,9,10\n"
            );
        }
    }

    #[test]
    fn klines_rejects_schema_violations() {
        let root = tempfile::tempdir().unwrap();
        let client = Client::tracked(build_rocket(root.path().into())).unwrap();
        for body in [
            r#"{"startDate":"2025-01-01T00:00"}"#,
            r#"{"symbol":"BTCUSDT"}"#,
            r#"{"startDate":null,"symbol":"BTCUSDT"}"#,
            r#"{"startDate":123,"symbol":"BTCUSDT"}"#,
            r#"{"startDate":"2025-01-01T00:00","extra":true,"symbol":"BTCUSDT"}"#,
            r#"{"startDate":"2026-11-16","symbol":"BTCUSDT"}"#,
            r#"{"startDate":"2026-11-16T24:00","symbol":"BTCUSDT"}"#,
            r#"{"startDate":"2026-11-16T14:60","symbol":"BTCUSDT"}"#,
            r#"{"startDate":"2025-01-01T00:00:00","symbol":"BTCUSDT"}"#,
            r#"{"startDate":"2025-01-01T00:00Z","symbol":"BTCUSDT"}"#,
            r#"{"startDate":"2025-01-01T00:00","endDate":"invalid","symbol":"BTCUSDT"}"#,
            r#"{"startDate":"2025-01-01T00:00","endDate":null,"symbol":"BTCUSDT"}"#,
            r#"{"startDate":"2025-01-01T00:00","symbol":123}"#,
            r#"{"startDate":"2025-01-01T00:00","symbol":null}"#,
            r#"{"startDate":"2025-02-30T00:00","symbol":"BTCUSDT"}"#,
            r#"{"startDate":"2025-01-01T00:01","endDate":"2025-01-01T00:00","symbol":"BTCUSDT"}"#,
            r#"{"startDate":"2025-01-01T00:00","symbol":"../BTCUSDT"}"#,
        ] {
            let response = client
                .post("/klines")
                .header(ContentType::JSON)
                .body(body)
                .dispatch();
            assert_eq!(response.status(), Status::UnprocessableEntity, "{body}");
        }
    }
    #[test]
    fn klines_handles_empty_and_corrupt_archives() {
        let root = tempfile::tempdir().unwrap();
        write_month(root.path(), 2025, 1, b"");
        let client = Client::tracked(build_rocket(root.path().into())).unwrap();
        let body =
            r#"{"startDate":"2025-01-01T00:00","endDate":"2025-01-01T00:01","symbol":"BTCUSDT"}"#;
        let response = client
            .post("/klines")
            .header(ContentType::JSON)
            .body(body)
            .dispatch();
        assert_eq!(response.status(), Status::Ok);
        assert_eq!(response.into_string().unwrap(), "");
        write_zip(root.path());
        let path = archive::monthly_kline_path(root.path(), "BTCUSDT", "BTCUSDT-1m-2025-01.zip");
        std::fs::write(path, b"broken zip").unwrap();
        let response = client
            .post("/klines")
            .header(ContentType::JSON)
            .body(body)
            .dispatch();
        assert_eq!(response.status(), Status::InternalServerError);
    }

    #[test]
    fn download_failure_falls_through_to_archive_retrieval() {
        let root = tempfile::tempdir().unwrap();
        let path = archive::monthly_kline_path(root.path(), "BTCUSDT", "BTCUSDT-1m-2025-01.zip");
        std::fs::create_dir_all(path).unwrap();
        let client = Client::tracked(build_rocket(root.path().into())).unwrap();
        let response = client.post("/klines").header(ContentType::JSON)
            .body(r#"{"startDate":"2025-01-01T00:00","endDate":"2025-01-01T00:01","symbol":"BTCUSDT"}"#)
            .dispatch();
        // The directory also fails retrieval, which retains its own HTTP 500.
        assert_eq!(response.status(), Status::InternalServerError);
        assert_eq!(response.into_string().unwrap(), "Archive retrieval failed");
    }

    #[test]
    fn jsonl_streams_only_first_six_columns_as_numbers() {
        let root = tempfile::tempdir().unwrap();
        write_month(root.path(), 2025, 1,
            b"1735689600000000,1234567890.123456789,2.50,0.10,1.25,100.00000001,unused,unused,unused,unused,unused,unused\n1735689660000000,6,7,8,9,10,ignored\n");
        let client = Client::tracked(build_rocket(root.path().into())).unwrap();
        let response = client.post("/klines-jsonl").header(ContentType::JSON)
            .body(r#"{"startDate":"2025-01-01T00:00","endDate":"2025-01-01T00:01","symbol":"BTCUSDT"}"#)
            .dispatch();
        assert_eq!(response.status(), Status::Ok);
        assert_eq!(
            response.content_type(),
            Some(ContentType::new("application", "x-ndjson"))
        );
        let body = response.into_string().unwrap();
        assert!(body.contains("1234567890.123456789"));
        assert!(body.ends_with('\n'));
        let rows: Vec<serde_json::Value> = body
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows.len(), 2);
        let names = ["open_time", "open", "high", "low", "close", "volume"];
        for row in &rows {
            let object = row.as_object().unwrap();
            assert_eq!(object.len(), 6);
            assert!(names.iter().all(|name| object[*name].is_number()));
        }
        assert_eq!(rows[0]["open_time"].as_u64(), Some(1735689600000000));
        assert_eq!(rows[1]["open_time"].as_u64(), Some(1735689660000000));
    }

    #[test]
    fn jsonl_handles_empty_results_and_rejects_invalid_rows_and_requests() {
        let root = tempfile::tempdir().unwrap();
        write_month(root.path(), 2025, 1, b"");
        let client = Client::tracked(build_rocket(root.path().into())).unwrap();
        let body =
            r#"{"startDate":"2025-01-01T00:00","endDate":"2025-01-01T00:01","symbol":"BTCUSDT"}"#;
        let response = client
            .post("/klines-jsonl")
            .header(ContentType::JSON)
            .body(body)
            .dispatch();
        assert_eq!(response.status(), Status::Ok);
        assert_eq!(response.into_string().unwrap(), "");
        for row in [
            "1735689600000000,1,2",
            "1735689600000000,NaN,2,3,4,5",
            "1735689600000000,1,2,3,4,Infinity",
        ] {
            write_month(root.path(), 2025, 1, row.as_bytes());
            let response = client
                .post("/klines-jsonl")
                .header(ContentType::JSON)
                .body(body)
                .dispatch();
            assert_eq!(response.status(), Status::InternalServerError);
        }
        let response = client
            .post("/klines-jsonl")
            .header(ContentType::JSON)
            .body(r#"{"startDate":"2025-01-01T00:00"}"#)
            .dispatch();
        assert_eq!(response.status(), Status::UnprocessableEntity);
    }

    #[test]
    fn jsonl_carry_forward_carries_values_only_between_real_rows() {
        let root = tempfile::tempdir().unwrap();
        write_month(
            root.path(),
            2025,
            1,
            b"1735689660000000,1.1234567890123456789,2,3,4,5\n1735689840000000,6,7,8,9,10\n",
        );
        let client = Client::tracked(build_rocket(root.path().into())).unwrap();
        let body =
            r#"{"startDate":"2025-01-01T00:00","endDate":"2025-01-01T00:05","symbol":"BTCUSDT"}"#;
        for (query, expected) in [
            ("", 2),
            ("?carry-forward=false", 2),
            ("?carry-forward=true", 4),
        ] {
            let response = client
                .post(format!("/klines-jsonl{query}"))
                .header(ContentType::JSON)
                .body(body)
                .dispatch();
            assert_eq!(response.status(), Status::Ok);
            let output = response.into_string().unwrap();
            let rows: Vec<serde_json::Value> = output
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(rows.len(), expected);
            assert_eq!(rows[0]["open_time"], 1735689660000000_u64);
            assert_eq!(rows.last().unwrap()["open_time"], 1735689840000000_u64);
            if query == "?carry-forward=true" {
                for (index, row) in rows.iter().enumerate() {
                    assert_eq!(
                        row["open_time"],
                        1735689660000000_u64 + index as u64 * 60_000_000
                    );
                }
                for row in &rows[1..3] {
                    for name in ["open", "high", "low", "close", "volume"] {
                        assert_eq!(row[name], rows[0][name]);
                    }
                }
                assert_eq!(rows[3]["open"], 6);
            }
        }
    }

    #[test]
    fn jsonl_carry_forward_handles_the_archive_timestamp_unit_transition() {
        let root = tempfile::tempdir().unwrap();
        write_month(root.path(), 2024, 12, b"1735689480000,1,2,3,4,5\n");
        write_month(root.path(), 2025, 1, b"1735689720000000,6,7,8,9,10\n");
        let client = Client::tracked(build_rocket(root.path().into())).unwrap();
        let response = client.post("/klines-jsonl?carry-forward=true").header(ContentType::JSON)
            .body(r#"{"startDate":"2024-12-31T23:58","endDate":"2025-01-01T00:02","symbol":"BTCUSDT"}"#).dispatch();
        assert_eq!(response.status(), Status::Ok);
        let output = response.into_string().unwrap();
        let rows: Vec<serde_json::Value> = output
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let timestamps: Vec<u64> = rows
            .iter()
            .map(|row| row["open_time"].as_u64().unwrap())
            .collect();
        assert_eq!(
            timestamps,
            [
                1735689480000,
                1735689540000,
                1735689600000000,
                1735689660000000,
                1735689720000000
            ]
        );
        for row in &rows[1..4] {
            assert_eq!(row["close"], 4);
        }
        assert_eq!(rows[4]["close"], 9);
    }

    #[test]
    fn jsonl_gain_uses_consecutive_rows_and_former_timestamps() {
        let root = tempfile::tempdir().unwrap();
        write_month(root.path(), 2025, 1, b"1735689600000000,100,200,80,100,0\n1735689660000000,125,100,100,100,5\n1735689720000000,100,200,80,50,0\n");
        let client = Client::tracked(build_rocket(root.path().into())).unwrap();
        let body =
            r#"{"startDate":"2025-01-01T00:00","endDate":"2025-01-01T00:02","symbol":"BTCUSDT"}"#;
        let response = client
            .post("/klines-jsonl?gain=true")
            .header(ContentType::JSON)
            .body(body)
            .dispatch();
        assert_eq!(response.status(), Status::Ok);
        let output = response.into_string().unwrap();
        let rows: Vec<serde_json::Value> = output
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            rows,
            vec![
                serde_json::json!({"open_time":1735689600000000_u64,"open":25.0,"high":-100.0,"low":25.0,"close":0.0,"volume":null}),
                serde_json::json!({"open_time":1735689660000000_u64,"open":-25.0,"high":100.0,"low":-25.0,"close":-100.0,"volume":null}),
            ]
        );
        let response = client
            .post("/klines-jsonl?gain=false")
            .header(ContentType::JSON)
            .body(body)
            .dispatch();
        assert_eq!(response.into_string().unwrap().lines().count(), 3);
    }

    #[test]
    fn jsonl_applies_carry_forward_before_gain_and_omits_unpaired_rows() {
        let root = tempfile::tempdir().unwrap();
        write_month(
            root.path(),
            2025,
            1,
            b"1735689600000000,100,100,100,100,100\n1735689720000000,125,125,125,125,125\n",
        );
        let client = Client::tracked(build_rocket(root.path().into())).unwrap();
        let body =
            r#"{"startDate":"2025-01-01T00:00","endDate":"2025-01-01T00:02","symbol":"BTCUSDT"}"#;
        let response = client
            .post("/klines-jsonl?carry-forward=true&gain=true")
            .header(ContentType::JSON)
            .body(body)
            .dispatch();
        assert_eq!(response.status(), Status::Ok);
        let output = response.into_string().unwrap();
        let rows: Vec<serde_json::Value> = output
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["open_time"], 1735689600000000_u64);
        assert_eq!(rows[1]["open_time"], 1735689660000000_u64);
        for field in ["open", "high", "low", "close", "volume"] {
            assert_eq!(rows[0][field], 0.0);
            assert_eq!(rows[1][field], 25.0);
        }
        for body in [
            r#"{"startDate":"2025-01-01T00:00","endDate":"2025-01-01T00:00","symbol":"BTCUSDT"}"#,
            r#"{"startDate":"2025-01-01T00:01","endDate":"2025-01-01T00:01","symbol":"BTCUSDT"}"#,
        ] {
            let response = client
                .post("/klines-jsonl?gain=true")
                .header(ContentType::JSON)
                .body(body)
                .dispatch();
            assert_eq!(response.status(), Status::Ok);
            assert_eq!(response.into_string().unwrap(), "");
        }
    }

    #[rocket::async_test]
    async fn streaming_errors_are_propagated_after_buffered_rows() {
        use rocket::tokio::io::AsyncReadExt;
        let (sender, receiver) = rocket::tokio::sync::mpsc::unbounded_channel();
        let publisher = rocket::tokio::spawn(async move {
            sender.send("row".into()).unwrap();
            Err(anyhow::anyhow!("archive read failed"))
        });
        let mut reader = super::KlineReader {
            format: super::OutputFormat::Csv,
            previous: None,
            next: None,
            receiver,
            publisher: Some(publisher),
            pending: std::io::Cursor::new(Vec::new()),
        };
        let mut output = Vec::new();
        assert!(reader.read_to_end(&mut output).await.is_err());
        assert_eq!(output, b"row\n");
    }
}
