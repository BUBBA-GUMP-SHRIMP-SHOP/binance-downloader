# archive

Shared archive directory conventions used by `binance-downloader` and
`kline-retriever`.

- `default_archive_root()` uses `ARCHIVE_BASE_PATH`, then the OS data directory,
  then the current directory, and appends `binance-archive`.
- `monthly_kline_path(root, symbol, filename)` builds the path
  `root/data/spot/monthly/klines/<symbol>/1m/<filename>`.

Callers validate symbols and filenames before constructing paths. An explicitly
configured root can be passed directly to `monthly_kline_path`.
