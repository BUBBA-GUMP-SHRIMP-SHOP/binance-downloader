mod display;
mod paths;
mod processor;

use anyhow::{Result, bail, ensure};
use std::{collections::HashSet, path::PathBuf, sync::Arc, time::Duration};
use tokio::{io::AsyncBufRead, sync::mpsc, task::JoinHandle};

/// Configuration shared by the CLI and embedded downloader.
pub struct DownloadOptions {
    pub destination: Option<PathBuf>,
    pub jobs: usize,
    pub no_progress: bool,
    pub quiet: bool,
}

impl Default for DownloadOptions {
    fn default() -> Self {
        Self {
            destination: None,
            jobs: 20,
            no_progress: true,
            quiet: false,
        }
    }
}

/// Cloneable, bounded queue handle. Drop all handles to drain and stop downloads.
#[derive(Clone)]
pub struct DownloadSender(mpsc::Sender<String>);

impl DownloadSender {
    /// Validate a Binance archive link and wait for queue capacity.
    /// Success means queued, not that the download has completed.
    pub async fn add_link(&self, link: impl Into<String>) -> Result<()> {
        let link = link.into();
        paths::FileInfo::parse(&link)?;
        self.0
            .send(link)
            .await
            .map_err(|_| anyhow::anyhow!("download channel is closed"))
    }
}

/// Start downloads on the current Tokio runtime. Await the returned task after
/// dropping all senders to finish queued work and observe download failures.
pub fn start_downloads(
    options: DownloadOptions,
    capacity: usize,
) -> Result<(DownloadSender, JoinHandle<Result<()>>)> {
    ensure!(capacity > 0, "channel capacity must be positive");
    let mut downloader = Downloader::new(options)?;
    let (sender, receiver) = mpsc::channel(capacity);
    let task = tokio::spawn(async move {
        let result =
            processor::run_channel(receiver, downloader.jobs, |line| downloader.download(line))
                .await;
        downloader.finish(result)
    });
    Ok((DownloadSender(sender), task))
}

/// Download links from newline-delimited input, preserving CLI input semantics.
pub async fn run<R: AsyncBufRead + Unpin>(reader: R, options: DownloadOptions) -> Result<()> {
    let mut downloader = Downloader::new(options)?;
    let result = processor::run(reader, downloader.jobs, |line| downloader.download(line)).await;
    downloader.finish(result)
}

struct Downloader {
    jobs: usize,
    destination: Arc<PathBuf>,
    client: reqwest::Client,
    display: Arc<display::Display>,
    seen: HashSet<paths::FileInfo>,
}

impl Downloader {
    fn new(options: DownloadOptions) -> Result<Self> {
        ensure!(options.jobs > 0, "parallelism must be positive");
        let destination = Arc::new(
            options
                .destination
                .unwrap_or_else(archive::default_archive_root),
        );
        let client = reqwest::Client::builder()
            .pool_max_idle_per_host(options.jobs)
            .http2_adaptive_window(true)
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(3600))
            .build()?;
        let display = Arc::new(display::Display::new(!options.no_progress, options.quiet));
        Ok(Self {
            jobs: options.jobs,
            destination,
            client,
            display,
            seen: HashSet::new(),
        })
    }

    fn download(
        &mut self,
        line: String,
    ) -> impl std::future::Future<Output = ()> + Send + 'static + use<> {
        let parsed = paths::FileInfo::parse(&line);
        let duplicate = parsed
            .as_ref()
            .is_ok_and(|info| !self.seen.insert(info.clone()));
        let destination = self.destination.clone();
        let client = self.client.clone();
        let display = self.display.clone();
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
                .download(&client, &destination, |bytes, total| {
                    display.progress(id, bytes, total)
                })
                .await;
            display.finish(id, outcome);
        }
    }

    fn finish(&self, result: Result<()>) -> Result<()> {
        let failed = self.display.summary();
        result?;
        if failed > 0 {
            bail!("{failed} download(s) failed");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINK: &str =
        "https://data.binance.vision/data/spot/monthly/klines/BTCUSDT/1m/BTCUSDT-1m-2026-04.zip";

    #[tokio::test(flavor = "current_thread")]
    async fn channel_validates_links_and_drains_after_all_senders_close() {
        let root = std::env::temp_dir().join(format!(
            "binance-channel-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = paths::FileInfo::parse(LINK).unwrap().local_path(&root);
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&path, b"existing archive").await.unwrap();
        let options = DownloadOptions {
            destination: Some(root.clone()),
            jobs: 1,
            quiet: true,
            ..Default::default()
        };
        let (sender, task) = start_downloads(options, 1).unwrap();
        assert!(
            sender
                .add_link("https://example.com/not-an-archive")
                .await
                .is_err()
        );
        let other = sender.clone();
        sender.add_link(LINK).await.unwrap();
        drop(sender);
        other.add_link(LINK).await.unwrap();
        drop(other);
        task.await.unwrap().unwrap();
        assert_eq!(tokio::fs::read(path).await.unwrap(), b"existing archive");
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn channel_rejects_zero_limits_and_reports_closed_receiver() {
        assert!(start_downloads(DownloadOptions::default(), 0).is_err());
        assert!(
            start_downloads(
                DownloadOptions {
                    jobs: 0,
                    ..Default::default()
                },
                1
            )
            .is_err()
        );
        let (sender, receiver) = mpsc::channel(1);
        drop(receiver);
        assert!(DownloadSender(sender).add_link(LINK).await.is_err());
    }
}
