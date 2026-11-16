use anyhow::{ensure, Context, Result};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::io::{AsyncWriteExt, BufWriter};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FileInfo {
    pub symbol: String,
    pub year: String,
    pub month: String,
    pub filename: String,
}

#[derive(Debug)]
pub enum Outcome {
    Downloaded,
    Existing,
    Missing,
}

impl FileInfo {
    pub fn parse(input: &str) -> Result<Self> {
        let url = url::Url::parse(input).context("invalid URL")?;
        ensure!(
            url.scheme() == "https"
                && url.host_str() == Some("data.binance.vision")
                && url.port().is_none()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "expected a Binance archive URL"
        );
        let parts: Vec<_> = url.path().split('/').collect();
        ensure!(
            parts.len() == 8
                && parts[1..5] == ["data", "spot", "monthly", "klines"]
                && parts[6] == "1m",
            "expected monthly 1m spot klines"
        );
        let symbol = parts[5];
        ensure!(
            !symbol.is_empty()
                && symbol
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'_'),
            "invalid symbol"
        );
        let filename = parts[7];
        Self::from_filename(symbol, filename)
    }

    fn from_filename(symbol: &str, filename: &str) -> Result<Self> {
        let prefix = format!("{symbol}-1m-");
        let date = filename
            .strip_prefix(&prefix)
            .and_then(|s| s.strip_suffix(".zip"))
            .context("filename must match symbol and interval")?;
        let (year, month) = date.split_once('-').context("missing archive date")?;
        ensure!(
            year.len() == 4
                && year.bytes().all(|c| c.is_ascii_digit())
                && month.len() == 2
                && month.parse::<u8>().is_ok_and(|m| (1..=12).contains(&m)),
            "invalid archive date"
        );
        Ok(Self {
            symbol: symbol.into(),
            year: year.into(),
            month: month.into(),
            filename: filename.into(),
        })
    }

    pub fn remote_url(&self) -> String {
        format!(
            "https://data.binance.vision/data/spot/monthly/klines/{}/1m/{}",
            self.symbol, self.filename
        )
    }

    pub fn local_path(&self, archive_root: &Path) -> PathBuf {
        archive_root
            .join("data/spot/monthly/klines")
            .join(&self.symbol)
            .join("1m")
            .join(&self.filename)
    }

    pub async fn download(
        &self,
        client: &reqwest::Client,
        archive_root: &Path,
        progress: impl FnMut(u64, Option<u64>),
    ) -> Result<Outcome> {
        download_to(
            client,
            &self.remote_url(),
            &self.local_path(archive_root),
            progress,
        )
        .await
    }
}

/// Preserve the existing environment/OS default when no CLI destination is given.
pub fn default_archive_root() -> PathBuf {
    std::env::var_os("ARCHIVE_BASE_PATH")
        .map(PathBuf::from)
        .or_else(dirs::data_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("binance-archive")
}

async fn download_to(
    client: &reqwest::Client,
    url: &str,
    path: &Path,
    mut progress: impl FnMut(u64, Option<u64>),
) -> Result<Outcome> {
    match tokio::fs::metadata(path).await {
        Ok(meta) if meta.is_file() && meta.len() > 0 => return Ok(Outcome::Existing),
        Ok(meta) => ensure!(
            meta.is_file(),
            "destination is not a file: {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut response = client.get(url).send().await.context("request failed")?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(Outcome::Missing);
    }
    response
        .error_for_status_ref()
        .context("server rejected download")?;
    let total = response.content_length();
    tokio::fs::create_dir_all(path.parent().context("missing destination directory")?).await?;
    let temporary = path.with_extension("zip.part");
    let file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .await
        .with_context(|| {
            format!(
                "cannot create {}; remove stale .part files before retrying",
                temporary.display()
            )
        })?;
    // Coalesce small HTTP frames into larger writes to the blocking filesystem pool.
    let mut file = BufWriter::with_capacity(256 * 1024, file);
    let result: Result<()> = async {
        let mut downloaded = 0;
        progress(0, total);
        let mut last_progress = Instant::now();
        while let Some(chunk) = response.chunk().await.context("reading response")? {
            file.write_all(&chunk).await?;
            downloaded += chunk.len() as u64;
            // Avoid contending on the shared display mutex for every network frame.
            if last_progress.elapsed() >= Duration::from_millis(100) {
                progress(downloaded, total);
                last_progress = Instant::now();
            }
        }
        ensure!(downloaded > 0, "empty response");
        if let Some(total) = total {
            ensure!(
                downloaded == total,
                "incomplete response: {downloaded}/{total} bytes"
            );
        }
        file.flush().await?;
        file.get_ref().sync_all().await?;
        progress(downloaded, total);
        Ok(())
    }
    .await;
    drop(file);
    if let Err(error) = result {
        tokio::fs::remove_file(&temporary)
            .await
            .context("removing incomplete download")?;
        return Err(error);
    }
    tokio::fs::rename(&temporary, path)
        .await
        .context("committing completed download")?;
    Ok(Outcome::Downloaded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_rebuilds_archive_url() {
        let url = "https://data.binance.vision/data/spot/monthly/klines/BTCUSDT/1m/BTCUSDT-1m-2026-04.zip";
        let info = FileInfo::parse(url).unwrap();
        assert_eq!(info.remote_url(), url);
        assert_eq!((info.year.as_str(), info.month.as_str()), ("2026", "04"));
        assert!(info.local_path(Path::new("binance-archive")).ends_with(
            "binance-archive/data/spot/monthly/klines/BTCUSDT/1m/BTCUSDT-1m-2026-04.zip"
        ));
        for bad in [
            url.replace("data.binance.vision", "example.com"),
            url.replace("-04.zip", "-13.zip"),
            url.replace("BTCUSDT-1m", "ETHUSDT-1m"),
            format!("{url}.html"),
        ] {
            assert!(FileInfo::parse(&bad).is_err(), "{bad}");
        }
    }
    async fn response_case(response: &[u8], expected: &str) {
        use std::io::{Read, Write};
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let wire_response = response.to_vec();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            let _ = stream.read(&mut request).unwrap();
            stream.write_all(&wire_response).unwrap();
        });
        let directory = std::env::temp_dir().join(format!(
            "binance-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let path = directory.join("archive.zip");
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let mut bytes = 0;
        let result = download_to(
            &client,
            &format!("http://{address}/archive.zip"),
            &path,
            |n, _| bytes = n,
        )
        .await;
        server.join().unwrap();
        match expected {
            "downloaded" => {
                assert!(matches!(result.unwrap(), Outcome::Downloaded));
                let body_start = response.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
                let body = &response[body_start..];
                assert_eq!(tokio::fs::read(&path).await.unwrap(), body);
                assert_eq!(bytes, body.len() as u64);
                // Existing files are skipped without contacting the now-closed server.
                assert!(matches!(
                    download_to(&client, &format!("http://{address}"), &path, |_, _| {})
                        .await
                        .unwrap(),
                    Outcome::Existing
                ));
            }
            "missing" => {
                assert!(matches!(result.unwrap(), Outcome::Missing));
                assert!(!path.exists());
            }
            _ => {
                assert!(result.is_err());
                assert!(!path.exists());
            }
        }
        assert!(!path.with_extension("zip.part").exists());
        if directory.exists() {
            tokio::fs::remove_dir_all(directory).await.unwrap();
        }
    }

    #[tokio::test]
    async fn streams_and_commits_then_skips_existing() {
        response_case(
            b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\nzip data",
            "downloaded",
        )
        .await;
    }

    #[tokio::test]
    async fn missing_and_http_errors_never_create_archives() {
        response_case(
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            "missing",
        )
        .await;
        response_case(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 4\r\nConnection: close\r\n\r\noops", "error").await;
    }

    #[tokio::test]
    async fn truncated_download_removes_partial_file() {
        response_case(
            b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\nshort",
            "error",
        )
        .await;
    }
    #[tokio::test]
    async fn buffered_download_flushes_full_blocks_and_tail() {
        let body: Vec<u8> = (0..(2 * 1024 * 1024 + 137))
            .map(|i| (i % 251) as u8)
            .collect();
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(&body);
        response_case(&response, "downloaded").await;
    }
}
