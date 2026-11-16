# kline-web-provider

A Rocket web server and starting point for serving kline data.

Run from the workspace root:

```bash
cargo run -p kline-web-provider
```

The server listens at `http://127.0.0.1:8000` by default:

- `GET /` returns the service name.
- `GET /health` returns `ok`.

- `POST /klines` validates and echoes a date range as JSON. It does not fetch kline data yet.

```bash
curl http://127.0.0.1:8000/klines \
  -H 'Content-Type: application/json' \
  -d '{"startDate":"2026-11-16T14:30","endDate":"2026-11-17T18:00","symbol":"BTCUSDT"}'
```

`startDate` and `symbol` are required; `endDate` may be omitted. Both must be strings in
`YYYY-MM-DDTHH:MM` format with hours 00–23 and minutes 00–59. Extra fields and
null values are rejected with HTTP 422. Malformed JSON returns HTTP 400.
As in the supplied schema, calendar validity and date ordering are not checked.

`symbol` is a required string (for example, `BTCUSDT`) and is echoed when provided.
