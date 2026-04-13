use anyhow::{Context, Result};
use regex::Regex;
use std::{env, path::PathBuf};

// const EXAMPLE: &str = "https://data.binance.vision/data/spot/monthly/klines/BTCUSDT/1m/BTCUSDT-1m-2026-04.zip";

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
    pub async fn remote_url_exists(&self) -> Result<bool> {
        http_client::remote_url_exists(self).await
    }
    pub fn local_path_exists(&self) -> Result<bool> {
        http_client::local_path_exists(self)
    }
    pub async fn download(&self) -> Result<bool> {
        http_client::download(self).await
    }
    pub fn create_local_file_for_writing(&self) -> Result<std::fs::File> {
        fs::create_dir_from_file_info(self)?;
        std::fs::File::create(self.local_path()).with_context(|| "Can't open file")
    }
}
impl From<&str> for FileInfo {
    fn from(url: &str) -> Self {
        http_client::parse_url(url).unwrap_or_else(|| panic!("Failed to parse URL: {}", url))
    }
}

mod fs {
    pub fn create_dir_from_file_info(file_info: &super::FileInfo) -> std::io::Result<()> {
        let dir_path = std::path::PathBuf::from(&file_info.local_path())
            .parent()
            .unwrap()
            .to_path_buf();
        std::fs::create_dir_all(dir_path)
    }
}

mod http_client {
    use anyhow::Result;

    pub async fn remote_url_exists(file_info: &super::FileInfo) -> Result<bool> {
        println!(
            "🚧 Testing Download:                 {}",
            file_info.remote_url()
        );
        let response = reqwest::get(&file_info.remote_url()).await?;
        let status = response.status();
        let bytes = response.bytes().await?;
        let local_file = file_info.create_local_file_for_writing()?;
        let mut local_file = tokio::fs::File::from_std(local_file);
        tokio::io::copy(&mut bytes.as_ref(), &mut local_file).await?;

        Ok(status.is_success())
    }
    pub fn local_path_exists(file_info: &super::FileInfo) -> Result<bool> {
        Ok(std::path::PathBuf::from(&file_info.local_path()).exists())
    }
    pub async fn download(file_info: &super::FileInfo) -> Result<bool> {
        println!(
            "✅️ Downloading:                      {}",
            file_info.remote_url()
        );
        let response = reqwest::get(&file_info.remote_url()).await?;
        let status = response.status();
        let bytes = response.bytes().await?;
        let local_file = file_info.create_local_file_for_writing()?;
        let mut local_file = tokio::fs::File::from_std(local_file);
        tokio::io::copy(&mut bytes.as_ref(), &mut local_file).await?;

        Ok(status.is_success())
    }

    pub fn parse_url(url: &str) -> Option<super::FileInfo> {
        let captures = super::BINANCE_KLINES_REGEX.captures(url)?;
        let symbol = captures.get(2)?.as_str().to_string();
        let year = captures.get(3)?.as_str().to_string();
        let month = captures.get(4)?.as_str().to_string();
        let filename = format!("{}-1m-{}-{}.zip", symbol, year, month);
        Some(super::FileInfo {
            symbol,
            month,
            year,
            filename,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{http_client, FileInfo};

    #[test]
    fn parse_url_extracts_fields() {
        let url = "https://data.binance.vision/data/spot/monthly/klines/BTCUSDT/1m/BTCUSDT-1m-2026-04.zip";
        let parsed = http_client::parse_url(url).expect("URL should match Binance kline pattern");

        assert_eq!(parsed.symbol, "BTCUSDT");
        assert_eq!(parsed.year, "2026");
        assert_eq!(parsed.month, "04");
        assert_eq!(parsed.filename, "BTCUSDT-1m-2026-04.zip");
    }

    #[test]
    fn parse_url_rejects_unexpected_url() {
        let invalid = "https://example.com/not-binance.zip";
        assert!(http_client::parse_url(invalid).is_none());
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
