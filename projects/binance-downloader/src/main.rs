use anyhow::Result;
use binance_downloader::{DownloadOptions, run};
use clap::Parser;
use tokio::io::{AsyncRead, BufReader};

#[derive(Parser)]
#[command(about = "Download Binance monthly 1-minute spot archives")]
struct Args {
    /// File containing one Binance archive URL per line; omit or use - for stdin
    filename: Option<String>,
    #[command(flatten)]
    download: binance_downloader::DownloadArgs,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let reader: Box<dyn AsyncRead + Send + Unpin> = match args.filename.as_deref() {
        None | Some("-") => Box::new(tokio::io::stdin()),
        Some(filename) => Box::new(tokio::fs::File::open(filename).await?),
    };
    run(BufReader::new(reader), DownloadOptions::from(args.download)).await
}
