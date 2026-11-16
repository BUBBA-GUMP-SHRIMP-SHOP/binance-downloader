mod gain;
mod rapido;

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
    interpolated: f64,
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
            interpolated: 0.0,
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
        interpolated: f64,
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
        interpolated: former.interpolated,
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
    remaining: Option<u64>,
    format: OutputFormat,
    previous: Option<KlineJson>,
    next: Option<KlineJson>,
    receiver: mpsc::UnboundedReceiver<String>,
    publisher: Option<JoinHandle<anyhow::Result<u64>>>,
    pending: io::Cursor<Vec<u8>>,
}

impl KlineReader {
    fn set_output(&mut self, bytes: Vec<u8>) {
        if !bytes.is_empty() {
            if let Some(remaining) = &mut self.remaining {
                *remaining -= 1;
            }
        }
        self.pending = io::Cursor::new(bytes);
    }

    fn set_line(&mut self, line: String) -> io::Result<()> {
        match self.format {
            OutputFormat::Csv => self.set_output(format!("{line}\n").into_bytes()),
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
                    filled.interpolated = 1.0;
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
        self.set_output(bytes);
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
            if self.remaining == Some(0) {
                self.receiver.close();
                if let Some(task) = self.publisher.take() {
                    task.abort();
                }
                self.next = None;
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
    #[serde(
        default,
        deserialize_with = "deserialize_optional",
        skip_serializing_if = "Option::is_none"
    )]
    end_date: Option<MinuteDateTime>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional",
        skip_serializing_if = "Option::is_none"
    )]
    start_date: Option<MinuteDateTime>,
    symbol: String,
    #[serde(
        default,
        deserialize_with = "deserialize_optional",
        skip_serializing_if = "Option::is_none"
    )]
    clusters: Option<std::num::NonZeroU64>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional",
        skip_serializing_if = "Option::is_none"
    )]
    window: Option<u64>,
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
    let reader = start_stream(range.into_inner(), root, OutputFormat::Csv, false).await?;
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
        false,
    )
    .await?;
    Ok((
        ContentType::new("application", "x-ndjson"),
        ReaderStream::one(reader),
    ))
}

#[rocket::post("/indi-jsonl", format = "json", data = "<range>")]
async fn indi_jsonl(
    range: Json<DateRange>,
    root: &State<ArchiveRoot>,
) -> Result<(ContentType, ReaderStream![rapido::Reader<KlineReader>]), ApiError> {
    let range = range.into_inner();
    let window = range
        .window
        .filter(|value| *value > 0)
        .ok_or_else(|| invalid("indi-jsonl requires a positive window"))?;
    let reader = start_stream(
        range,
        root,
        OutputFormat::Jsonl {
            carry_forward: true,
            gain: true,
        },
        true,
    )
    .await?;
    Ok((
        ContentType::new("application", "x-ndjson"),
        ReaderStream::one(rapido::Reader::new(reader, window)),
    ))
}

fn clamp_date_range(
    earlier: PrimitiveDateTime,
    later: PrimitiveDateTime,
) -> Result<PrimitiveDateTime, ApiError> {
    if earlier > later {
        return Err(invalid("endDate must not follow startDate"));
    }
    let (year, month) = if later.month() == time::Month::January {
        (later.year() - 1, time::Month::December)
    } else {
        (later.year(), later.month().previous())
    };
    // Clamp month-end dates, e.g. March 31 maps to February 28 or 29.
    let mut day = later.day();
    let earliest_date = loop {
        if let Ok(date) = time::Date::from_calendar_date(year, month, day) {
            break date;
        }
        if day == 1 {
            // The previous month is outside the representable date range.
            return Ok(earlier);
        }
        day -= 1;
    };
    Ok(earlier.max(PrimitiveDateTime::new(earliest_date, later.time())))
}

fn apply_window(
    earlier: PrimitiveDateTime,
    later: PrimitiveDateTime,
    window: Option<u64>,
) -> PrimitiveDateTime {
    if let Some(minutes) = window {
        if i128::from(minutes) * 60_000_000_000 < (later - earlier).whole_nanoseconds() {
            // The comparison bounds minutes to the supported date span.
            return later - time::Duration::minutes(minutes as i64);
        }
    }
    earlier
}

fn expand_clusters(
    earlier: PrimitiveDateTime,
    later: PrimitiveDateTime,
    clusters: u64,
) -> Result<PrimitiveDateTime, ApiError> {
    let duration = (later - earlier)
        .whole_nanoseconds()
        .checked_mul(i128::from(clusters))
        .ok_or_else(|| invalid("clusters extends beyond the supported date range"))?;
    let timestamp = later
        .assume_utc()
        .unix_timestamp_nanos()
        .checked_sub(duration)
        .ok_or_else(|| invalid("clusters extends beyond the supported date range"))?;
    let date = time::OffsetDateTime::from_unix_timestamp_nanos(timestamp)
        .map_err(|_| invalid("clusters extends beyond the supported date range"))?;
    Ok(PrimitiveDateTime::new(date.date(), date.time()))
}

async fn start_stream(
    range: DateRange,
    root: &ArchiveRoot,
    format: OutputFormat,
    require_downloads: bool,
) -> Result<KlineReader, ApiError> {
    let start = match &range.end_date {
        Some(date) => parse_date(&date.0).map_err(invalid)?,
        // Clamping chooses one calendar month before the resolved startDate.
        None => PrimitiveDateTime::MIN,
    };
    let end = match &range.start_date {
        Some(date) => parse_date(&date.0).map_err(invalid)?,
        None => {
            let now = time::OffsetDateTime::now_utc();
            PrimitiveDateTime::new(now.date(), now.time())
        }
    };
    let start = clamp_date_range(start, end)?;
    if range.symbol.is_empty()
        || !range
            .symbol
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err(invalid("invalid symbol"));
    }
    if range.window == Some(0) {
        return Ok(KlineReader {
            remaining: Some(0),
            format,
            previous: None,
            next: None,
            receiver: mpsc::unbounded_channel().1,
            publisher: None,
            pending: io::Cursor::new(Vec::new()),
        });
    }
    let start = apply_window(start, end, range.window);
    let clusters = range.clusters.map_or(1, |value| value.get());
    let window = range
        .window
        .map(|window| {
            window
                .checked_mul(clusters)
                .ok_or_else(|| invalid("window times clusters exceeds the supported line count"))
        })
        .transpose()?;
    let start = expand_clusters(start, end, clusters)?;
    // Keep the gate until downloads finish even if this request disconnects.
    let download_root = root.0.clone();
    let download_symbol = range.symbol.clone();
    let gate = root.1.clone();
    // Finish preparation before creating a publisher or reading any rows.
    let preparation = tokio::spawn(async move {
        let _guard = gate.lock().await;
        ensure_downloaded(&download_root, &download_symbol, start, end).await
    })
    .await;
    if require_downloads {
        preparation.map_err(archive_error)?.map_err(|error| {
            eprintln!("Required archive download failed: {error:#}");
            status::Custom(
                Status::ServiceUnavailable,
                "Required archive download failed".into(),
            )
        })?;
    }
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
        remaining: window,
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
        .mount(
            "/",
            routes![index, health, klines, klines_jsonl, indi_jsonl],
        )
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
        for body in
            [r#"{"endDate":"2025-01-01T00:00","startDate":"2025-01-01T00:01","symbol":"BTCUSDT"}"#]
        {
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
    fn date_ranges_are_clamped_to_one_calendar_month() {
        for (earlier, later, expected) in [
            ("2025-01-01T12:00", "2025-02-01T12:00", "2025-01-01T12:00"),
            ("2025-01-01T11:59", "2025-02-01T12:00", "2025-01-01T12:00"),
            ("2025-01-01T00:00", "2025-03-31T12:00", "2025-02-28T12:00"),
            ("2024-01-01T00:00", "2024-03-31T12:00", "2024-02-29T12:00"),
            ("2024-01-01T00:00", "2025-01-31T12:00", "2024-12-31T12:00"),
            ("2025-01-15T12:00", "2025-02-01T12:00", "2025-01-15T12:00"),
            ("2025-01-01T12:00", "2025-01-01T12:00", "2025-01-01T12:00"),
        ] {
            assert_eq!(
                super::clamp_date_range(
                    super::parse_date(earlier).unwrap(),
                    super::parse_date(later).unwrap(),
                )
                .unwrap(),
                super::parse_date(expected).unwrap()
            );
        }
    }

    #[test]
    fn endpoints_trim_ranges_longer_than_one_month() {
        let root = tempfile::tempdir().unwrap();
        // One row before the clamped boundary and one exactly at it.
        write_month(
            root.path(),
            2025,
            1,
            b"1735689600000000,1,2,3,4,5\n1735689660000000,6,7,8,9,10\n",
        );
        write_month(root.path(), 2025, 2, b"");
        let client = Client::tracked(build_rocket(root.path().into())).unwrap();
        for route in ["/klines", "/klines-jsonl"] {
            let response = client.post(route).header(ContentType::JSON)
                .body(r#"{"endDate":"2020-01-01T00:00","startDate":"2025-02-01T00:01","symbol":"BTCUSDT","clusters":1}"#)
                .dispatch();
            assert_eq!(response.status(), Status::Ok);
            let body = response.into_string().unwrap();
            assert!(!body.contains("1735689600000000"));
            assert!(body.contains("1735689660000000"));
        }
    }

    #[test]
    fn endpoints_default_omitted_end_date_to_one_month_before_start() {
        let root = tempfile::tempdir().unwrap();
        let now = time::OffsetDateTime::now_utc();
        for year in [now.year() - 1, now.year()] {
            for month in 1..=12 {
                write_month(root.path(), year, month, b"");
            }
        }
        write_month(
            root.path(),
            2025,
            1,
            b"1735689600000000,1,2,3,4,5\n1735689660000000,6,7,8,9,10\n",
        );
        write_month(root.path(), 2025, 2, b"");
        let client = Client::tracked(build_rocket(root.path().into())).unwrap();
        for route in ["/klines", "/klines-jsonl"] {
            let response = client
                .post(route)
                .header(ContentType::JSON)
                .body(r#"{"startDate":"2025-02-01T00:01","symbol":"BTCUSDT"}"#)
                .dispatch();
            assert_eq!(response.status(), Status::Ok);
            let body = response.into_string().unwrap();
            assert!(!body.contains("1735689600000000"));
            assert!(body.contains("1735689660000000"));
            let response = client
                .post(route)
                .header(ContentType::JSON)
                .body(r#"{"symbol":"BTCUSDT"}"#)
                .dispatch();
            assert_eq!(response.status(), Status::Ok);
        }
    }

    #[test]
    fn window_limits_output_lines_within_the_date_range() {
        let root = tempfile::tempdir().unwrap();
        write_month(root.path(), 2025, 1,
            b"1735689600000000,1,2,3,4,5\n1735689660000000,6,7,8,9,10\n1735689780000000,6,7,8,9,10\n");
        let client = Client::tracked(build_rocket(root.path().into())).unwrap();
        for (route, available, last_minute) in [
            ("/klines", 2, "01"),
            ("/klines-jsonl", 2, "01"),
            ("/klines-jsonl?carry-forward=true", 4, "03"),
            ("/klines-jsonl?gain=true", 1, "01"),
            ("/klines-jsonl?carry-forward=true&gain=true", 3, "03"),
        ] {
            for window in [0, 1, 2, 3, 100] {
                let body = format!(
                    r#"{{"endDate":"2025-01-01T00:00","startDate":"2025-01-01T00:{last_minute}","symbol":"BTCUSDT","window":{window}}}"#
                );
                let response = client
                    .post(route)
                    .header(ContentType::JSON)
                    .body(body)
                    .dispatch();
                assert_eq!(response.status(), Status::Ok);
                assert_eq!(
                    response.into_string().unwrap().lines().count(),
                    if route == "/klines-jsonl?carry-forward=true&gain=true" && window == 1 {
                        // Only the final real row lies in this shortened range.
                        0
                    } else {
                        window.min(available)
                    }
                );
            }
            for window in ["-1", "1.5", "null", "\"2\""] {
                let body = format!(r#"{{"symbol":"BTCUSDT","window":{window}}}"#);
                assert_eq!(
                    client
                        .post(route)
                        .header(ContentType::JSON)
                        .body(body)
                        .dispatch()
                        .status(),
                    Status::UnprocessableEntity
                );
            }
        }
    }

    #[test]
    fn ten_line_window_with_three_clusters_returns_thirty_lines() {
        let root = tempfile::tempdir().unwrap();
        let rows: String = (0..=60)
            .map(|minute| {
                format!(
                    "{},1,2,3,4,5\n",
                    1_735_689_600_000_000u64 + minute * 60_000_000
                )
            })
            .collect();
        write_month(root.path(), 2025, 1, rows.as_bytes());
        let client = Client::tracked(build_rocket(root.path().into())).unwrap();
        for route in [
            "/klines",
            "/klines-jsonl",
            "/klines-jsonl?gain=true",
            "/klines-jsonl?carry-forward=true&gain=true",
        ] {
            let response = client.post(route).header(ContentType::JSON)
                .body(r#"{"symbol":"BTCUSDT","startDate":"2025-01-01T01:00","window":10,"clusters":3}"#)
                .dispatch();
            assert_eq!(response.status(), Status::Ok);
            let body = response.into_string().unwrap();
            assert_eq!(body.lines().count(), 30);
            // startDate 01:00 minus (10 minutes * 3) = endDate 00:30.
            assert!(body.lines().next().unwrap().contains("1735691400000000"));
        }
    }

    #[test]
    fn indi_jsonl_aggregates_three_windows_after_carry_forward_and_gain() {
        let root = tempfile::tempdir().unwrap();
        let rows: String = (0..=30)
            .map(|index| {
                let value = 2u64.pow(index);
                format!(
                    "{},{value},{value},{value},{value},{value}\n",
                    1_735_689_600_000_000u64 + u64::from(index) * 120_000_000
                )
            })
            .collect();
        write_month(root.path(), 2025, 1, rows.as_bytes());
        let client = Client::tracked(build_rocket(root.path().into())).unwrap();
        let response = client
            .post("/indi-jsonl")
            .header(ContentType::JSON)
            .body(r#"{"symbol":"BTCUSDT","startDate":"2025-01-01T01:00","window":10,"clusters":3}"#)
            .dispatch();
        assert_eq!(response.status(), Status::Ok);
        assert_eq!(
            response.content_type(),
            Some(ContentType::new("application", "x-ndjson"))
        );
        let body = response.into_string().unwrap();
        let windows: Vec<serde_json::Value> = body
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(windows.len(), 3);
        for (index, window) in windows.iter().enumerate() {
            assert_eq!(
                window["open_time"],
                1_735_691_400_000_000u64 + index as u64 * 600_000_000
            );
            assert_eq!(window["window"], 10);
            assert_eq!(window["interpolated"], 0.5);
            for field in ["s_open", "s_low", "v_open", "v_low"] {
                assert_eq!(window[field], 0.0);
            }
            for field in ["s_high", "s_close", "v_high", "v_close"] {
                assert_eq!(window[field], 100.0);
            }
        }
    }

    #[test]
    fn indi_jsonl_requires_all_downloads_before_aggregation() {
        let root = tempfile::tempdir().unwrap();
        // The older month contains sufficient data to produce an aggregate,
        // but preparation of the later required month must fail first.
        write_zip(root.path());
        let blocked = archive::monthly_kline_path(root.path(), "BTCUSDT", "BTCUSDT-1m-2025-02.zip");
        std::fs::create_dir_all(blocked).unwrap();
        let client = Client::tracked(build_rocket(root.path().into())).unwrap();
        let response = client
            .post("/indi-jsonl")
            .header(ContentType::JSON)
            .body(r#"{"symbol":"BTCUSDT","startDate":"2025-02-01T00:00","window":44640}"#)
            .dispatch();
        assert_eq!(response.status(), Status::ServiceUnavailable);
        assert_eq!(
            response.into_string().unwrap(),
            "Required archive download failed"
        );
    }

    #[test]
    fn indi_jsonl_requires_window_and_omits_incomplete_groups() {
        let root = tempfile::tempdir().unwrap();
        write_zip(root.path());
        write_month(root.path(), 2024, 12, b"");
        let client = Client::tracked(build_rocket(root.path().into())).unwrap();
        for body in [
            r#"{"symbol":"BTCUSDT"}"#,
            r#"{"symbol":"BTCUSDT","window":0}"#,
        ] {
            assert_eq!(
                client
                    .post("/indi-jsonl")
                    .header(ContentType::JSON)
                    .body(body)
                    .dispatch()
                    .status(),
                Status::UnprocessableEntity
            );
        }
        let response = client.post("/indi-jsonl").header(ContentType::JSON)
            .body(r#"{"symbol":"BTCUSDT","endDate":"2025-01-01T00:00","startDate":"2025-01-01T00:01","window":10,"clusters":3}"#).dispatch();
        assert_eq!(response.status(), Status::Ok);
        assert_eq!(response.into_string().unwrap(), "");
    }

    #[test]
    fn clusters_are_applied_after_other_checks() {
        let root = tempfile::tempdir().unwrap();
        let client = Client::tracked(build_rocket(root.path().into())).unwrap();
        for route in ["/klines", "/klines-jsonl"] {
            // This multiplier would overflow the supported date range if applied.
            let response = client
                .post(route)
                .header(ContentType::JSON)
                .body(r#"{"symbol":"BTCUSDT","window":0,"clusters":18446744073709551615}"#)
                .dispatch();
            assert_eq!(response.status(), Status::Ok);
            assert_eq!(response.into_string().unwrap(), "");
            let response = client
                .post(route)
                .header(ContentType::JSON)
                .body(r#"{"symbol":"../BTCUSDT","clusters":18446744073709551615}"#)
                .dispatch();
            assert_eq!(response.status(), Status::UnprocessableEntity);
            assert_eq!(response.into_string().unwrap(), "invalid symbol");
        }
    }

    #[test]
    fn window_recalculates_end_date_before_clusters() {
        let end = super::parse_date("2025-01-01T01:00").unwrap();
        let earlier = super::parse_date("2025-01-01T00:00").unwrap();
        let start = super::apply_window(earlier, end, Some(10));
        assert_eq!(start, super::parse_date("2025-01-01T00:50").unwrap());
        assert_eq!(
            super::expand_clusters(start, end, 3).unwrap(),
            super::parse_date("2025-01-01T00:30").unwrap()
        );
        assert_eq!(super::apply_window(start, end, Some(100)), start);
        assert_eq!(super::apply_window(start, end, None), start);
        assert_eq!(super::apply_window(start, end, Some(u64::MAX)), start);
    }

    #[test]
    fn clusters_expand_after_clamping() {
        let later = super::parse_date("2025-03-31T12:00").unwrap();
        let earlier = super::parse_date("2020-01-01T00:00").unwrap();
        let clamped = super::clamp_date_range(earlier, later).unwrap();
        assert_eq!(super::expand_clusters(clamped, later, 1).unwrap(), clamped);
        assert_eq!(
            super::expand_clusters(clamped, later, 3).unwrap(),
            later - (later - clamped) * 3
        );
        assert!(super::expand_clusters(clamped, later, u64::MAX).is_err());
    }

    #[test]
    fn clusters_expand_endpoint_ranges_and_window_limit() {
        let root = tempfile::tempdir().unwrap();
        write_month(root.path(), 2025, 1,
            b"1735689600000000,1,2,3,4,5\n1735689660000000,6,7,8,9,10\n1735689720000000,6,7,8,9,10\n1735689780000000,6,7,8,9,10\n");
        let client = Client::tracked(build_rocket(root.path().into())).unwrap();
        for route in ["/klines", "/klines-jsonl"] {
            for (clusters, window, expected) in [(1, 100, 2), (3, 100, 4), (3, 2, 4)] {
                let body = format!(
                    r#"{{"symbol":"BTCUSDT","endDate":"2025-01-01T00:02","startDate":"2025-01-01T00:03","clusters":{clusters},"window":{window}}}"#
                );
                let response = client
                    .post(route)
                    .header(ContentType::JSON)
                    .body(body)
                    .dispatch();
                assert_eq!(response.status(), Status::Ok);
                assert_eq!(response.into_string().unwrap().lines().count(), expected);
            }
            for clusters in ["0", "-1", "1.5", "null", "\"3\"", "{}"] {
                let body = format!(r#"{{"symbol":"BTCUSDT","clusters":{clusters}}}"#);
                assert_eq!(
                    client
                        .post(route)
                        .header(ContentType::JSON)
                        .body(body)
                        .dispatch()
                        .status(),
                    Status::UnprocessableEntity
                );
            }
        }
    }

    #[test]
    fn klines_rejects_schema_violations() {
        let root = tempfile::tempdir().unwrap();
        let client = Client::tracked(build_rocket(root.path().into())).unwrap();
        for body in [
            r#"{"endDate":"2025-01-01T00:00"}"#,
            r#"{"endDate":null,"symbol":"BTCUSDT"}"#,
            r#"{"endDate":123,"symbol":"BTCUSDT"}"#,
            r#"{"endDate":"2025-01-01T00:00","extra":true,"symbol":"BTCUSDT"}"#,
            r#"{"endDate":"2026-11-16","symbol":"BTCUSDT"}"#,
            r#"{"endDate":"2026-11-16T24:00","symbol":"BTCUSDT"}"#,
            r#"{"endDate":"2026-11-16T14:60","symbol":"BTCUSDT"}"#,
            r#"{"endDate":"2025-01-01T00:00:00","symbol":"BTCUSDT"}"#,
            r#"{"endDate":"2025-01-01T00:00Z","symbol":"BTCUSDT"}"#,
            r#"{"endDate":"2025-01-01T00:00","startDate":"invalid","symbol":"BTCUSDT"}"#,
            r#"{"endDate":"2025-01-01T00:00","startDate":null,"symbol":"BTCUSDT"}"#,
            r#"{"endDate":"2025-01-01T00:00","symbol":123}"#,
            r#"{"endDate":"2025-01-01T00:00","symbol":null}"#,
            r#"{"endDate":"2025-02-30T00:00","symbol":"BTCUSDT"}"#,
            r#"{"endDate":"2025-01-01T00:01","startDate":"2025-01-01T00:00","symbol":"BTCUSDT"}"#,
            r#"{"endDate":"2025-01-01T00:00","symbol":"../BTCUSDT"}"#,
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
            r#"{"endDate":"2025-01-01T00:00","startDate":"2025-01-01T00:01","symbol":"BTCUSDT"}"#;
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
            .body(r#"{"endDate":"2025-01-01T00:00","startDate":"2025-01-01T00:01","symbol":"BTCUSDT"}"#)
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
            .body(r#"{"endDate":"2025-01-01T00:00","startDate":"2025-01-01T00:01","symbol":"BTCUSDT"}"#)
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
            assert_eq!(object.len(), 7);
            assert_eq!(object["interpolated"], 0.0);
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
            r#"{"endDate":"2025-01-01T00:00","startDate":"2025-01-01T00:01","symbol":"BTCUSDT"}"#;
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
            .body(r#"{"endDate":"2025-01-01T00:00"}"#)
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
            r#"{"endDate":"2025-01-01T00:00","startDate":"2025-01-01T00:05","symbol":"BTCUSDT"}"#;
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
                assert_eq!(rows[0]["interpolated"], 0.0);
                assert_eq!(rows[3]["interpolated"], 0.0);
                for row in &rows[1..3] {
                    assert_eq!(row["interpolated"], 1.0);
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
            .body(r#"{"endDate":"2024-12-31T23:58","startDate":"2025-01-01T00:02","symbol":"BTCUSDT"}"#).dispatch();
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
            r#"{"endDate":"2025-01-01T00:00","startDate":"2025-01-01T00:02","symbol":"BTCUSDT"}"#;
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
                serde_json::json!({"open_time":1735689600000000_u64,"open":25.0,"high":-100.0,"low":25.0,"close":0.0,"volume":null,"interpolated":0.0}),
                serde_json::json!({"open_time":1735689660000000_u64,"open":-25.0,"high":100.0,"low":-25.0,"close":-100.0,"volume":null,"interpolated":0.0}),
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
            r#"{"endDate":"2025-01-01T00:00","startDate":"2025-01-01T00:02","symbol":"BTCUSDT"}"#;
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
        assert_eq!(rows[0]["interpolated"], 0.0);
        assert_eq!(rows[1]["interpolated"], 1.0);
        for field in ["open", "high", "low", "close", "volume"] {
            assert_eq!(rows[0][field], 0.0);
            assert_eq!(rows[1][field], 25.0);
        }
        for body in [
            r#"{"endDate":"2025-01-01T00:00","startDate":"2025-01-01T00:00","symbol":"BTCUSDT"}"#,
            r#"{"endDate":"2025-01-01T00:01","startDate":"2025-01-01T00:01","symbol":"BTCUSDT"}"#,
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
            remaining: None,
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
