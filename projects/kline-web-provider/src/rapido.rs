use rocket::serde::{Deserialize, Serialize};
use rocket::tokio::io::{AsyncBufRead, AsyncRead, BufReader, ReadBuf};
use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};

#[derive(Deserialize)]
#[serde(crate = "rocket::serde")]
struct Row {
    open_time: u64,
    interpolated: f64,
    open: Option<f64>,
    high: Option<f64>,
    low: Option<f64>,
    close: Option<f64>,
    volume: Option<f64>,
}

#[derive(Serialize)]
#[serde(crate = "rocket::serde")]
struct Window {
    open_time: u64,
    window: u64,
    interpolated: f64,
    s_open: Option<f64>,
    s_high: Option<f64>,
    s_low: Option<f64>,
    s_close: Option<f64>,
    v_open: Option<f64>,
    v_high: Option<f64>,
    v_low: Option<f64>,
    v_close: Option<f64>,
}

// Undefined gains remain null for an extremum if any member is undefined.
fn maximum(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    Some(a?.max(b?))
}
fn minimum(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    Some(a?.min(b?))
}

pub(crate) struct Reader<R> {
    source: BufReader<R>,
    size: u64,
    count: u64,
    aggregate: Option<Window>,
    line: Vec<u8>,
    pending: io::Cursor<Vec<u8>>,
}

impl<R: AsyncRead + Unpin> Reader<R> {
    pub(crate) fn new(source: R, size: u64) -> Self {
        Self {
            source: BufReader::new(source),
            size,
            count: 0,
            aggregate: None,
            line: Vec::new(),
            pending: io::Cursor::new(Vec::new()),
        }
    }

    fn accept_line(&mut self) -> io::Result<()> {
        let row: Row = serde_json::from_slice(&self.line).map_err(io::Error::other)?;
        self.line.clear();
        if let Some(window) = &mut self.aggregate {
            window.interpolated += row.interpolated;
            window.s_high = maximum(window.s_high, row.high);
            window.s_low = minimum(window.s_low, row.low);
            window.s_close = row.close;
            window.v_high = maximum(window.v_high, row.volume);
            window.v_low = minimum(window.v_low, row.volume);
            window.v_close = row.volume;
        } else {
            self.aggregate = Some(Window {
                open_time: row.open_time,
                window: self.size,
                interpolated: row.interpolated,
                s_open: row.open,
                s_high: row.high,
                s_low: row.low,
                s_close: row.close,
                v_open: row.volume,
                v_high: row.volume,
                v_low: row.volume,
                v_close: row.volume,
            });
        }
        self.count += 1;
        if self.count == self.size {
            let mut window = self.aggregate.take().unwrap();
            window.interpolated /= self.size as f64;
            let mut bytes = serde_json::to_vec(&window).map_err(io::Error::other)?;
            bytes.push(b'\n');
            self.pending = io::Cursor::new(bytes);
            self.count = 0;
        }
        Ok(())
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for Reader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        loop {
            let before = buf.filled().len();
            std::task::ready!(Pin::new(&mut self.pending).poll_read(cx, buf))?;
            if buf.filled().len() > before {
                return Poll::Ready(Ok(()));
            }
            let this = &mut *self;
            let bytes = std::task::ready!(Pin::new(&mut this.source).poll_fill_buf(cx))?;
            if bytes.is_empty() {
                // Emit complete windows only; an incomplete final group is omitted.
                return Poll::Ready(Ok(()));
            }
            let newline = bytes.iter().position(|byte| *byte == b'\n');
            let used = newline.map_or(bytes.len(), |index| index + 1);
            this.line.extend_from_slice(&bytes[..used]);
            Pin::new(&mut this.source).consume(used);
            if newline.is_some() {
                this.accept_line()?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Reader;
    use rocket::tokio::io::AsyncReadExt;

    #[rocket::async_test]
    async fn aggregates_signed_gains_and_preserves_nulls() {
        let input = b"{\"open_time\":1,\"interpolated\":0,\"open\":5,\"high\":-2,\"low\":-4,\"close\":1,\"volume\":8}\n{\"open_time\":2,\"interpolated\":1,\"open\":9,\"high\":3,\"low\":-8,\"close\":-7,\"volume\":-2}\n{\"open_time\":3,\"interpolated\":0,\"open\":null,\"high\":null,\"low\":null,\"close\":null,\"volume\":null}\n";
        let mut reader = Reader::new(&input[..], 2);
        let mut output = String::new();
        reader.read_to_string(&mut output).await.unwrap();
        assert_eq!(output.lines().count(), 1);
        let row: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
        assert_eq!(
            row,
            serde_json::json!({"open_time":1,"window":2,"interpolated":0.5,
            "s_open":5.0,"s_high":3.0,"s_low":-8.0,"s_close":-7.0,
            "v_open":8.0,"v_high":8.0,"v_low":-2.0,"v_close":-2.0})
        );
        let mut reader = Reader::new(&input[..], 3);
        output.clear();
        reader.read_to_string(&mut output).await.unwrap();
        let row: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
        for field in ["s_high", "s_low", "s_close", "v_high", "v_low", "v_close"] {
            assert!(row[field].is_null());
        }
    }

    #[rocket::async_test]
    async fn malformed_upstream_rows_propagate_read_errors() {
        let mut reader = Reader::new(&b"invalid json\n"[..], 1);
        assert!(reader.read_to_end(&mut Vec::new()).await.is_err());
    }
}
