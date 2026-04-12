use std::{env, path::PathBuf};
use regex::Regex;
use anyhow::{Context, Result};


const EXAMPLE: &str = "https://data.binance.vision/data/spot/monthly/klines/BTCUSDT/1m/BTCUSDT-1m-2026-04.zip";

const BINANCE_BASE_URL: &str = "https://data.binance.vision/";

lazy_static::lazy_static! {

    pub static ref BINANCE_KLINES_REGEX: Regex =
    Regex::new(r"/data/spot/monthly/klines/(\w+?)/1m/(\w+?)-1m-(\d{4})-(\d{2}).zip").unwrap();

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
        format!("{}/data/spot/monthly/klines/{}/1m/{}", BINANCE_BASE_URL, self.symbol, self.filename)
    }
    pub fn local_path(&self) -> String {
        format!("{}/data/spot/monthly/klines/{}/1m/{}", *ARCHIVE_BASE_PATH, self.symbol, self.filename)
    }
}
impl FileInfo {
    pub async fn remote_url_exists(url: &str) -> Result<bool> {
        http_client::remote_url_exists(url).await
    }
    pub fn create_local_file_for_writing(&self) -> Result<std::fs::File> {
        paths::create_dir_from_file_info(self)?;
        std::fs::File::create(self.local_path()).with_context(|| { "Can't open file" })
    }
}
impl From<&str> for FileInfo {
    fn from(url: &str) -> Self {
        http_client::parse_url(url).unwrap_or_else(|| panic!("Failed to parse URL: {}", url))
    }
}

mod paths {
    pub fn create_dir_from_file_info(file_info: &super::FileInfo) -> std::io::Result<()> {
        let dir_path = std::path::PathBuf::from(&file_info.local_path())
            .parent().unwrap()
            .to_path_buf()
            ;
        std::fs::create_dir_all(dir_path)
    }
}

mod http_client {
    use anyhow::Result;

    use async_curl::CurlActor;
    use curl_http_client::*;
    use http::{Method, Request};

    pub async fn remote_url_exists(url: &str) -> Result<bool> {
        let actor = CurlActor::new();
        let collector = Collector::Ram(Vec::new());

        let request = Request::builder()
            .uri(url)
            .method(Method::HEAD)
            .body(None)
            .unwrap();

        let response = HttpClient::new(collector)
            .request(request).unwrap()
            .nonblocking(actor)
            .perform()
            .await.unwrap();

        Ok(response.status().is_success())
    }

    pub fn parse_url(url: &str) -> Option<super::FileInfo> {
        let captures = super::BINANCE_KLINES_REGEX.captures(url)?;
        let symbol = captures.get(2)?.as_str().to_string();
        let year = captures.get(3)?.as_str().to_string();
        let month = captures.get(4)?.as_str().to_string();
        let filename = format!("{}-1m-{}-{}.zip", symbol, year, month);
        Some(super::FileInfo { symbol, month, year, filename })
    }

}