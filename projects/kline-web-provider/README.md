# kline-web-provider

A Rocket web server and starting point for serving kline data.

Run from the workspace root:

```bash
cargo run -p kline-web-provider
```

The server listens at `http://127.0.0.1:8000` by default:

- `GET /` returns the service name.
- `GET /health` returns `ok`.

- `POST /klines` streams matching archived rows as `text/csv`, in chronological month order.

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
Only existing monthly one-minute spot archives are read. Missing months are
skipped; a range with no rows returns HTTP 200 with an empty CSV body.

Extra fields, nulls, invalid dates, and reversed ranges return HTTP 422; malformed
JSON returns HTTP 400. Archive errors before the first matching row return HTTP
500. Errors after streaming starts terminate the body with a read error, since
HTTP headers have already been sent. Client disconnects stop the publisher by
dropping the channel receiver.
