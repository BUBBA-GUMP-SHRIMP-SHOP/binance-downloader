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
    Jsonl,
}

#[derive(Serialize)]
#[serde(crate = "rocket::serde")]
struct KlineJson {
    open_time: u64,
    open: serde_json::Number,
    high: serde_json::Number,
    low: serde_json::Number,
    close: serde_json::Number,
    volume: serde_json::Number,
}

impl OutputFormat {
    fn encode(self, line: String) -> io::Result<Vec<u8>> {
        let mut bytes = match self {
            Self::Csv => line.into_bytes(),
            Self::Jsonl => {
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
                let record = KlineJson {
                    open_time,
                    open: number()?,
                    high: number()?,
                    low: number()?,
                    close: number()?,
                    volume: number()?,
                };
                serde_json::to_vec(&record).map_err(io::Error::other)?
            }
        };
        bytes.push(b'\n');
        Ok(bytes)
    }
}

// Propagate publisher failures as body read errors if streaming has begun.
struct KlineReader {
    format: OutputFormat,
    receiver: mpsc::UnboundedReceiver<String>,
    publisher: Option<JoinHandle<anyhow::Result<u64>>>,
    pending: io::Cursor<Vec<u8>>,
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
            match self.receiver.poll_recv(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(line)) => {
                    self.pending = io::Cursor::new(self.format.encode(line)?)
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

#[rocket::post("/klines-jsonl", format = "json", data = "<range>")]
async fn klines_jsonl(
    range: Json<DateRange>,
    root: &State<ArchiveRoot>,
) -> Result<(ContentType, ReaderStream![KlineReader]), ApiError> {
    let reader = start_stream(range.into_inner(), root, OutputFormat::Jsonl).await?;
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
    let pending = first
        .map(|line| format.encode(line))
        .transpose()
        .map_err(archive_error)?
        .unwrap_or_default();
    Ok(KlineReader {
        format,
        receiver,
        publisher,
        pending: io::Cursor::new(pending),
    })
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
            receiver,
            publisher: Some(publisher),
            pending: std::io::Cursor::new(Vec::new()),
        };
        let mut output = Vec::new();
        assert!(reader.read_to_end(&mut output).await.is_err());
        assert_eq!(output, b"row\n");
    }
}
