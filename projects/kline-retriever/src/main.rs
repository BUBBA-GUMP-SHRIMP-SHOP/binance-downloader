use anyhow::{Context, Result, bail};
use kline_retriever::{parse_date, publish_lines};
use std::io::{self, Write};
use tokio::sync::mpsc;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if !(3..=4).contains(&args.len()) {
        bail!(
            "usage: kline-retriever SYMBOL START_DATE END_DATE [ARCHIVE_ROOT] (dates: YYYY-MM-DDTHH:MM, UTC)"
        );
    }
    let start = parse_date(&args[1])?;
    let end = parse_date(&args[2])?;
    let root = args
        .get(3)
        .map(Into::into)
        .unwrap_or_else(archive::default_archive_root);
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let publisher = tokio::spawn(publish_lines(root, args[0].clone(), start, end, sender));
    let mut stdout = io::BufWriter::new(io::stdout());
    while let Some(line) = receiver.recv().await {
        writeln!(stdout, "{line}")?;
    }
    stdout.flush()?;
    publisher.await.context("publisher task failed")??;
    Ok(())
}
