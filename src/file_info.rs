use anyhow::{Context, Result};
use async_curl::CurlActor;
use curl_http_client::{Collector, HttpClient};
use http::{Method, Request};
use regex::Regex;
use scrape_core::ContentType;
use std::{env, path::PathBuf};

const BINANCE_BASE_URL: &str = "https://data.binance.vision/";

lazy_static::lazy_static! {

    pub static ref BINANCE_ZIP_FILE_REGEX: Regex =
        Regex::new(r"(\w+?)-1m-(\d{4})-(\d{2}).zip").unwrap();


    pub static ref BINANCE_KLINES_REGEX: Regex =
        Regex::new(
            &format!(r"/data/spot/monthly/klines/(\w+?)/1m/{}",
                BINANCE_ZIP_FILE_REGEX.as_str()
            )
        ).unwrap();

    pub static ref ARCHIVE_BASE_PATH: String = {
        let key = "ARCHIVE_BASE_PATH";
        match env::var(key) {
            Ok(val) => {
                PathBuf::from(val)
            }
            Err(e) => {
                println!("couldn't interpret {key}: {e}");
                dirs::data_dir().unwrap()
            }
        }.join("binance-archive").to_str().unwrap().to_string()
    };
}

#[derive(Debug, Default, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileInfo {
    pub symbol: String,
    pub month: String,
    pub year: String,
    pub filename: String,
}
impl FileInfo {
    pub fn remote_url(&self) -> String {
        format!(
            "{}data/spot/monthly/klines/{}/1m/{}",
            BINANCE_BASE_URL, self.symbol, self.filename
        )
    }
    pub fn local_path(&self) -> String {
        format!(
            "{}/data/spot/monthly/klines/{}/1m/{}",
            *ARCHIVE_BASE_PATH, self.symbol, self.filename
        )
    }
}
impl FileInfo {
    pub fn create_local_file_for_writing(&self) -> Result<std::fs::File> {
        self.create_dir()?;
        std::fs::File::create(self.local_path()).with_context(|| "Can't open file")
    }
    pub fn create_dir(&self) -> std::io::Result<()> {
        let dir_path = std::path::PathBuf::from(self.local_path())
            .parent()
            .unwrap()
            .to_path_buf();
        std::fs::create_dir_all(dir_path)
    }

    pub async fn remote_url_exists(&self) -> Result<bool> {
        println!(
            "🚧 Testing, if Remote URL exists:                 {}",
            self.remote_url()
        );

        let actor = CurlActor::new();
        let collector = Collector::Ram(Vec::new());

        let request = Request::builder()
            .uri(self.remote_url())
            .method(Method::HEAD)
            .body(None)?;

        let response = HttpClient::new(collector)
            .request(request)
            .unwrap()
            .nonblocking(actor)
            .perform()
            .await
            .map_err(|e| anyhow::anyhow!("HTTP request failed: {}", e))?;

        let status = response.status();

        // let local_file = file_info.create_local_file_for_writing()?;
        // let mut local_file = tokio::fs::File::from_std(local_file);
        // tokio::io::copy(&mut bytes.as_ref(), &mut local_file).await?;

        // tokio::time::sleep(time::Duration::from_secs(1)).await;

        Ok(status.is_success())
    }

    pub fn local_path_exists(&self) -> Result<bool> {
        println!(
            "🚧 Testing Local Path:                 {}",
            self.local_path()
        );

        Ok(std::path::PathBuf::from(&self.local_path()).exists())
    }

    pub async fn download_zip(&self) -> Result<bool> {
        if self.local_path_exists()? {
            println!(
                "⚠️  File already exists locally, skipping: {}",
                self.local_path()
            );
            return Ok(true);
        }

        println!("✅️ Downloading:                      {}", self.remote_url());
        let actor = CurlActor::new();
        let collector = Collector::Ram(Vec::new());

        let request = Request::builder()
            .uri(self.remote_url())
            .method(Method::HEAD)
            .body(None)?;

        let response = HttpClient::new(collector)
            .request(request)
            .unwrap()
            .nonblocking(actor)
            .perform()
            .await
            .map_err(|e| anyhow::anyhow!("Download HTTP request failed: {}", e))?;

        let status = response.status();

        let local_file = self.create_local_file_for_writing()?;
        let mut local_file = tokio::fs::File::from_std(local_file);
        let body = response
            .body()
            .clone()
            .ok_or(anyhow::anyhow!("Response body is empty"))?;
        let mut cursor = std::io::Cursor::new(body);
        tokio::io::copy(&mut cursor, &mut local_file).await?;

        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        Ok(status.is_success())
    }
    pub async fn download_html(url: &str) -> Result<(bool, String)> {
        println!("✅️ Downloading html file:            {}", url);
        let actor = CurlActor::new();
        let collector = Collector::Ram(Vec::new());

        let request = Request::builder()
            .uri(url)
            .method(Method::HEAD)
            .body(None)?;

        let response = HttpClient::new(collector)
            .request(request)
            .unwrap()
            .nonblocking(actor)
            .perform()
            .await
            .map_err(|e| anyhow::anyhow!("Download HTTP request failed: {}", e))?;

        let status = response.status();

        let body = response
            .body()
            .clone()
            .ok_or(anyhow::anyhow!("Response body is empty"))?;

        let content = String::from_utf8(body)
            .map_err(|e| anyhow::anyhow!("Failed to parse response body as UTF-8: {}", e))?;

        Ok((status.is_success(), content))
    }
}
impl From<&str> for FileInfo {
    fn from(url: &str) -> Self {
        (|| {
            let captures = BINANCE_KLINES_REGEX.captures(url)?;
            let symbol = captures.get(2)?.as_str().to_string();
            let year = captures.get(3)?.as_str().to_string();
            let month = captures.get(4)?.as_str().to_string();
            let filename = format!("{}-1m-{}-{}.zip", symbol, year, month);
            Some(FileInfo {
                symbol,
                month,
                year,
                filename,
            })
        })()
        .unwrap_or_else(|| panic!("Failed to parse URL: {}", url))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_url_extracts_fields() {
        let url = "https://data.binance.vision/data/spot/monthly/klines/BTCUSDT/1m/BTCUSDT-1m-2026-04.zip";
        let parsed: FileInfo = url.into();

        assert_eq!(parsed.symbol, "BTCUSDT");
        assert_eq!(parsed.year, "2026");
        assert_eq!(parsed.month, "04");
        assert_eq!(parsed.filename, "BTCUSDT-1m-2026-04.zip");
    }

    #[test]
    fn parse_url_rejects_unexpected_url() {
        let invalid = "https://example.com/not-binance.zip";
        assert!(std::panic::catch_unwind(|| {
            let _: FileInfo = invalid.into();
        })
        .is_err());
    }

    #[test]
    fn remote_url_is_built_from_file_info() {
        let file_info = FileInfo {
            symbol: "ETHUSDT".to_string(),
            month: "01".to_string(),
            year: "2025".to_string(),
            filename: "ETHUSDT-1m-2025-01.zip".to_string(),
        };

        assert_eq!(
            file_info.remote_url(),
            "https://data.binance.vision/data/spot/monthly/klines/ETHUSDT/1m/ETHUSDT-1m-2025-01.zip"
        );
    }

    #[test]
    fn local_path_contains_expected_suffix() {
        let file_info = FileInfo {
            symbol: "SOLUSDT".to_string(),
            month: "12".to_string(),
            year: "2024".to_string(),
            filename: "SOLUSDT-1m-2024-12.zip".to_string(),
        };

        assert!(file_info
            .local_path()
            .ends_with("/data/spot/monthly/klines/SOLUSDT/1m/SOLUSDT-1m-2024-12.zip"));
    }
}
