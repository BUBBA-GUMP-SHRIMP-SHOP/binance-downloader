//! Shared archive directory conventions for downloaders and retrievers.

use std::path::{Path, PathBuf};

/// Resolve the archive root from `ARCHIVE_BASE_PATH`, the OS data directory,
/// or the current directory, appending `binance-archive`.
pub fn default_archive_root() -> PathBuf {
    std::env::var_os("ARCHIVE_BASE_PATH")
        .map(PathBuf::from)
        .or_else(dirs::data_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("binance-archive")
}

/// Construct a monthly one-minute spot archive path under an archive root.
/// `symbol` and `filename` must be validated archive path components.
pub fn monthly_kline_path(archive_root: &Path, symbol: &str, filename: &str) -> PathBuf {
    archive_root
        .join("data/spot/monthly/klines")
        .join(symbol)
        .join("1m")
        .join(filename)
}
