# Testing

This project includes unit tests for the URL/path parsing logic and the processor pipeline wiring helpers.

## Run tests

```bash
cargo test
```

## Current unit test coverage

### paths

Tests in `src/paths.rs` validate:

- Binance kline URL parsing into `FileInfo` fields (`symbol`, `year`, `month`, `filename`)
- Rejection of non-matching URLs
- Construction of expected remote URLs
- Local archive path suffix structure for generated file paths

### processor

Tests in `src/processor.rs` validate:

- `ProcessorUnit::process` applies the provided transform function
- `ProcessorUnit::connect` stores an input receiver in the internal set
- `ProcessConnector::connect_to` invokes downstream processor connection

## Notes

- These are deterministic unit tests and do not make real network requests.
- No integration tests are currently defined for download/network behavior.
