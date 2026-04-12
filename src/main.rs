
pub(crate) mod paths;

use anyhow::Result;
use crossbeam::channel::bounded;
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, BufReader, AsyncRead};


const PIPE_SIZE: usize = 10;


#[tokio::main]
async fn main() -> Result<()> {

    let filename = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("Usage: binance-downloader <path_to_file_with_urls>, e.g. ./data/data.txt. '-' for <stdio>.");
        std::process::exit(1);
    });
    let (url_writer, url_reader) = bounded(PIPE_SIZE);

    let file_handler = {
        let filename = filename.clone();
        tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Handle::current();
            rt.block_on(async {
                let file_reader: BufReader<Box<dyn AsyncRead + Unpin>> = if filename == "-" {
                    BufReader::new(Box::new(tokio::io::stdin()))
                } else {
                    BufReader::new(Box::new(File::open(filename).await?))
                };
                let mut file_reader = file_reader;
                let mut line = String::new();
                while file_reader.read_line(&mut line).await? > 0 {
                    url_writer.send(line.clone())?;
                    line.clear();
                }
                Ok::<(), anyhow::Error>(())
            })
        })
    };

    let (not_remote_writer, not_remote_reader) = bounded(PIPE_SIZE);
    let (download_writer, download_reader) = bounded(PIPE_SIZE);

    let url_handler = tokio::spawn(async move {
        for url in url_reader.iter() {
            let file_info = paths::FileInfo::from(url.as_str());
            if file_info.local_path_exists().unwrap_or(false) {
                println!("🤟 File already exists locally:      {}", file_info.local_path());
                continue;
            }
            if file_info.remote_url_exists().await.unwrap_or(false) {
                download_writer.send(file_info)?;
            } else {
                not_remote_writer.send(file_info)?;
            }
        }
        Ok::<(), anyhow::Error>(())
    });

    let not_remote_handler = tokio::spawn(async move {
        for file_info in not_remote_reader.iter() {
            println!("❌ Remote URL does not exist:        {}", file_info.remote_url());
        }
        Ok::<(), anyhow::Error>(())
    });

    let download_handler = {
        tokio::spawn(async move {
            for file_info in download_reader.iter() {
                file_info.download().await?;
            }
            Ok::<(), anyhow::Error>(())
        })
    };

    let _ = url_handler.await?;
    let _ = not_remote_handler.await?;
    let _ = download_handler.await?;
    let _ = file_handler.await?;

    Ok(())
}






