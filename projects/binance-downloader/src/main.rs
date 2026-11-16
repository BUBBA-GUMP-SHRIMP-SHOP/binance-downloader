use anyhow::Result;
use binance_downloader::{DownloadOptions, run};
use clap::Parser;
use std::path::PathBuf;
use tokio::io::{AsyncRead, BufReader};

#[derive(Parser)]
#[command(about = "Download Binance monthly 1-minute spot archives")]
struct Args {
    /// File containing one Binance archive URL per line; omit or use - for stdin
    filename: Option<String>,
    /// Archive root directory (files go under PATH/data/spot/monthly/klines)
    #[arg(short = 'd', long, value_name = "PATH")]
    destination: Option<PathBuf>,
    /// Maximum simultaneous downloads
    #[arg(short = 'j', long, default_value = "20", value_parser = clap::value_parser!(u16).range(1..=256))]
    jobs: u16,
    /// Print completion lines instead of an interactive display
    #[arg(long)]
    no_progress: bool,
    /// Suppress progress and routine messages; errors remain visible
    #[arg(short, long)]
    quiet: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let reader: Box<dyn AsyncRead + Send + Unpin> = match args.filename.as_deref() {
        None | Some("-") => Box::new(tokio::io::stdin()),
        Some(filename) => Box::new(tokio::fs::File::open(filename).await?),
    };
    run(
        BufReader::new(reader),
        DownloadOptions {
            destination: args.destination,
            jobs: args.jobs as usize,
            no_progress: args.no_progress,
            quiet: args.quiet,
        },
    )
    .await
}
