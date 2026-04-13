use anyhow::Result;
use tokio::{
    fs::File,
    io::{AsyncBufReadExt, AsyncRead, BufReader},
};

use crate::{file_info::FileInfo, pipeline::Event};

// ============================================================================
// FILE READER STAGE
// Reads URLs from file or stdin and feeds into pipeline
// ============================================================================
pub async fn file_reader(
    filename: String,
    url_tx: crossbeam::channel::Sender<Event<String>>,
) -> Result<()> {
    let file_reader: BufReader<Box<dyn AsyncRead + Unpin>> = if filename == "-" {
        BufReader::new(Box::new(tokio::io::stdin()))
    } else {
        BufReader::new(Box::new(File::open(filename).await?))
    };

    let mut file_reader = file_reader;
    let mut line = String::new();

    while file_reader.read_line(&mut line).await? > 0 {
        url_tx.send(Event::Data(line.clone()))?;
        line.clear();
    }

    Ok(())
}

// ============================================================================
// EXISTENCE & ROUTING STAGE
// ============================================================================
pub async fn parallel_existence_and_route(
    url_rx: crossbeam::channel::Receiver<Event<String>>,
    download_tx: crossbeam::channel::Sender<Event<FileInfo>>,
    not_remote_tx: crossbeam::channel::Sender<Event<FileInfo>>,
    parallelism: usize,
) -> anyhow::Result<()> {
    crate::processor::run_parallel(url_rx, parallelism, move |event| {
        let dl = download_tx.clone();
        let nr = not_remote_tx.clone();
        async move {
            if let Event::Data(url) = event {
                let file_info = crate::processor_unit::url_parser(&url);
                crate::processor_unit::existence_checker(
                    &file_info.clone(),
                    dl.clone(),
                    nr.clone(),
                )
                .await?;
                dl.send(Event::Data(file_info))?;
                Ok(())
            } else {
                Ok(())
            }
        }
    })
    .await
}

// ============================================================================
// NOT-FOUND LOGGING STAGE
// ============================================================================
pub async fn not_found_logging(
    not_remote_rx: crossbeam::channel::Receiver<Event<FileInfo>>,
) -> anyhow::Result<()> {
    tokio::task::spawn_blocking(move || {
        for event in not_remote_rx {
            match event {
                Event::Data(file_info) => {
                    crate::processor_unit::not_found_logger(&file_info);
                }
                Event::End => break,
            }
        }
    })
    .await?;
    Ok(())
}

// ============================================================================
// DOWNLOAD STAGE
// ============================================================================
pub async fn parallel_downloader(
    download_rx: crossbeam::channel::Receiver<Event<FileInfo>>,
    parallelism: usize,
) -> anyhow::Result<()> {
    crate::processor::run_parallel(download_rx, parallelism, |event| async move {
        if let Event::Data(file_info) = event {
            crate::processor_unit::downloader(&file_info).await
        } else {
            Ok(())
        }
    })
    .await
}
