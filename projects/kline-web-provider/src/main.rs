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
    path::PathBuf,
    pin::Pin,
    task::{Context, Poll},
};

struct ArchiveRoot(PathBuf);

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

// Propagate publisher failures as body read errors if streaming has begun.
struct KlineReader {
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
                    self.pending = io::Cursor::new(format!("{line}\n").into_bytes())
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
    let reader = KlineReader {
        receiver,
        publisher,
        pending: io::Cursor::new(
            first
                .map(|line| format!("{line}\n").into_bytes())
                .unwrap_or_default(),
        ),
    };
    Ok((ContentType::new("text", "csv"), ReaderStream::one(reader)))
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
        .manage(ArchiveRoot(root))
        .mount("/", routes![index, health, klines])
}

#[cfg(test)]
mod tests {
    use super::build_rocket;
    use rocket::{
        http::{ContentType, Status},
        local::blocking::Client,
    };

    fn write_zip(root: &std::path::Path) {
        use std::io::Write;
        let path = archive::monthly_kline_path(root, "BTCUSDT", "BTCUSDT-1m-2025-01.zip");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut zip = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        zip.start_file(
            "BTCUSDT-1m-2025-01.csv",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        zip.write_all(b"1735689600000000,1,2,3,4,5\n1735689660000000,6,7,8,9,10\n")
            .unwrap();
        zip.finish().unwrap();
    }

    #[test]
    fn klines_accepts_date_ranges() {
        let root = tempfile::tempdir().unwrap();
        let client = Client::tracked(build_rocket(root.path().into())).unwrap();
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

    #[rocket::async_test]
    async fn streaming_errors_are_propagated_after_buffered_rows() {
        use rocket::tokio::io::AsyncReadExt;
        let (sender, receiver) = rocket::tokio::sync::mpsc::unbounded_channel();
        let publisher = rocket::tokio::spawn(async move {
            sender.send("row".into()).unwrap();
            Err(anyhow::anyhow!("archive read failed"))
        });
        let mut reader = super::KlineReader {
            receiver,
            publisher: Some(publisher),
            pending: std::io::Cursor::new(Vec::new()),
        };
        let mut output = Vec::new();
        assert!(reader.read_to_end(&mut output).await.is_err());
        assert_eq!(output, b"row\n");
    }
}
