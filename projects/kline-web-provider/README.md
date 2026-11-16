# kline-web-provider

A Rocket web server and starting point for serving kline data.

Run from the workspace root:

```bash
cargo run -p kline-web-provider
```

The server listens at `http://127.0.0.1:8000` by default:

- `GET /` returns the service name.
- `GET /health` returns `ok`.

- `POST /klines` downloads required monthly archives first, then streams matching
  rows as `text/csv` in chronological month order.

```bash
curl http://127.0.0.1:8000/klines \
  -H 'Content-Type: application/json' \
  -d '{"startDate":"2026-11-16T14:30","endDate":"2026-11-17T18:00","symbol":"BTCUSDT"}'
```

`startDate` and `symbol` are required; `endDate` may be omitted and defaults to
current UTC time. Dates use `YYYY-MM-DDTHH:MM` and are interpreted as UTC. The
inclusive range filters rows by their opening timestamp. Symbols may contain only
ASCII letters, digits, and underscores, and must not be empty.

The archive root follows the shared `archive` crate: `ARCHIVE_BASE_PATH`, then the
OS data directory, then the current directory, with `binance-archive` appended.
Before streaming, the endpoint queues every month touched by the inclusive range
through `binance-downloader` and waits for completion. Existing nonempty files are
reused. Download preparation is serialized across requests to prevent conflicting
writes, and continues to completion if a waiting request disconnects.

Download failures and unavailable archives (including Binance HTTP 404) are
silently skipped in the HTTP response. The endpoint streams the locally available
rows, even when some months are missing. If no rows are available, it returns
HTTP 200 with an empty CSV body.

Extra fields, nulls, invalid dates, and reversed ranges return HTTP 422; malformed
JSON returns HTTP 400. Archive errors before the first matching row return HTTP
500. Errors after streaming starts terminate the body with a read error, since
HTTP headers have already been sent. Client disconnects stop the publisher by
dropping the channel receiver.

## JSON Lines

`POST /klines-jsonl` accepts the same JSON request as `/klines` and reuses its
validation, download preparation, and chronological archive stream. Download
failures remain silent and only available data is returned.

```bash
curl --no-buffer http://127.0.0.1:8000/klines-jsonl \
  -H 'Content-Type: application/json' \
  -d '{"startDate":"2025-01-01T00:00","endDate":"2025-01-01T00:01","symbol":"BTCUSDT"}'
```

The response is `application/x-ndjson`, one JSON object per line, containing only
CSV columns 0–5 as JSON numbers:

```json
{"open_time":1735689600000000,"open":1.25,"high":2.5,"low":1.0,"close":2.0,"volume":100.5}
```

`open_time` preserves the archive's timestamp unit (milliseconds before 2025,
microseconds from January 2025). Decimal values retain their precision as JSON
numbers. Invalid numeric values or missing columns fail with HTTP 500 before the
first row, or a body read error if streaming has begun. `/klines` continues to
return the original CSV rows.

### Fill missing minutes

Add `?carry-forward=true` to `/klines-jsonl` to fill missing one-minute steps between
returned rows. Each inserted row gets the missing `open_time` and copies all five
other fields from the preceding row. The next real row replaces those values.
Filling is incremental, including gaps across ZIP files and the 2025 timestamp
unit transition. It does not add rows before the first real row or after the last.
Omitting `carry-forward` or using `carry-forward=false` returns only archived rows.

```bash
curl --no-buffer 'http://127.0.0.1:8000/klines-jsonl?carry-forward=true' \
  -H 'Content-Type: application/json' \
  -d '{"startDate":"2025-01-01T00:00","endDate":"2025-01-01T00:10","symbol":"BTCUSDT"}'
```

### Gain

Add `?gain=true` to return the GAIN percentage for consecutive rows, using
[Romeo Disca's GAIN formula](https://romeo-disca.medium.com/a-more-robust-kpi-for-profits-and-losses-c0aa5ea115c2).
For each of `open`, `high`, `low`, `close`, and `volume`, let `r = later / former`:

- `100 * (r - 1)` when `r >= 1`.
- `100 * (1 - 1/r)` when `r < 1`.

Each result keeps the former row's `open_time`. N input rows produce N−1 gain
rows; empty or single-row inputs produce an empty response. Combine with
`?carry-forward=true&gain=true` to fill gaps before calculating gain. Unchanged
carried values produce zero gain.

The calculation lives in `src/gain.rs`. Derived gains use finite floating-point
numbers truncated toward zero to at most five decimal places (without padding
trailing zeros). Equal nonnegative values, including two zeros, produce zero. Undefined
zero transitions, negative values, and non-finite calculations produce JSON `null`
for that field. Normal JSONL output keeps its original decimal precision.
