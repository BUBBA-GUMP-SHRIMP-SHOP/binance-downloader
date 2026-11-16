mod display;
mod paths;
mod processor;

use anyhow::{Result, bail};
use clap::Parser;
use std::{collections::HashSet, sync::Arc, time::Duration};
use tokio::io::{AsyncRead, BufReader};

#[derive(Parser)]
#[command(about = "Download Binance monthly 1-minute spot archives")]
struct Args {
    /// File containing one Binance archive URL per line; omit or use - for stdin
    filename: Option<String>,
    /// Maximum simultaneous downloads
    #[arg(short = 'j', long, default_value = "20", value_parser = clap::value_parser!(u16).range(1..=256))]
    jobs: u16,
    /// Print completion lines instead of an interactive display
    #[arg(long)]
    no_progress: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let reader: Box<dyn AsyncRead + Send + Unpin> = match args.filename.as_deref() {
        None | Some("-") => Box::new(tokio::io::stdin()),
        Some(filename) => Box::new(tokio::fs::File::open(filename).await?),
    };
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(args.jobs as usize)
        .http2_adaptive_window(true)
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(3600))
        .build()?;
    let display = Arc::new(display::Display::new(!args.no_progress));
    let mut seen = HashSet::new();
    let result = processor::run(BufReader::new(reader), args.jobs as usize, |line| {
        let parsed = paths::FileInfo::parse(&line);
        let duplicate = parsed.as_ref().is_ok_and(|info| !seen.insert(info.clone()));
        let client = client.clone();
        let display = display.clone();
        async move {
            if duplicate {
                display.duplicate();
                return;
            }
            let info = match parsed {
                Ok(info) => info,
                Err(error) => {
                    display.invalid(format!("{line}: {error}"));
                    return;
                }
            };
            let id = display.start(info.filename.clone());
            let outcome = info
                .download(&client, |bytes, total| display.progress(id, bytes, total))
                .await;
            display.finish(id, outcome);
        }
    })
    .await;
    let failed = display.summary();
    result?;
    if failed > 0 {
        bail!("{failed} download(s) failed");
    }
    Ok(())
}
