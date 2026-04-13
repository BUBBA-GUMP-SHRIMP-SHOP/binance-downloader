
pub(crate) mod processor;
pub(crate) mod paths;

use anyhow::Result;
use crossbeam::channel::{bounded, Receiver};
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, BufReader, AsyncRead};


const PIPE_SIZE: usize = 10;

// ============================================================================
// PROCESSING UNIT: URL Parser
// Converts raw URL strings into FileInfo structures
// ============================================================================
fn pu_url_parser(url: &String) -> paths::FileInfo {
    paths::FileInfo::from(url.as_str())
}

// ============================================================================
// PROCESSING UNIT: Existence Checker
// Checks local/remote existence and routes via side effects
// Routes: locally existing -> log only
//         remote exists -> download channel
//         remote missing -> not-remote channel
// ============================================================================
async fn pu_existence_checker(
    file_info: &paths::FileInfo,
    download_tx: crossbeam::channel::Sender<paths::FileInfo>,
    not_remote_tx: crossbeam::channel::Sender<paths::FileInfo>,
) -> Result<()> {
    if file_info.local_path_exists().unwrap_or(false) {
        println!("🤟 File already exists locally:      {}", file_info.local_path());
        return Ok(());
    }

    if file_info.remote_url_exists().await.unwrap_or(false) {
        download_tx.send(file_info.clone())?;
    } else {
        not_remote_tx.send(file_info.clone())?;
    }

    Ok(())
}

// ============================================================================
// PROCESSING UNIT: Not Found Logger
// Logs FileInfo entries where remote URL was not found
// ============================================================================
fn pu_not_found_logger(file_info: &paths::FileInfo) -> () {
    println!("❌ Remote URL does not exist:        {}", file_info.remote_url());
}

// ============================================================================
// PROCESSING UNIT: Downloader
// Downloads files from remote URL to local path
// ============================================================================
async fn pu_downloader(file_info: &paths::FileInfo) -> Result<()> {
    file_info.download().await?;
    Ok(())
}

// ============================================================================
// FILE READER STAGE
// Reads URLs from file or stdin and feeds into pipeline
// ============================================================================
async fn stage_file_reader(filename: String, url_tx: crossbeam::channel::Sender<String>) -> Result<()> {
    let file_reader: BufReader<Box<dyn AsyncRead + Unpin>> = if filename == "-" {
        BufReader::new(Box::new(tokio::io::stdin()))
    } else {
        BufReader::new(Box::new(File::open(filename).await?))
    };

    let mut file_reader = file_reader;
    let mut line = String::new();

    while file_reader.read_line(&mut line).await? > 0 {
        url_tx.send(line.clone())?;
        line.clear();
    }

    Ok(())
}

// ============================================================================
// EXISTENCE & ROUTING STAGE
// Parses URLs to FileInfo, checks existence, routes to appropriate handler
// ============================================================================
async fn stage_existence_and_route(
    url_rx: Receiver<String>,
    download_tx: crossbeam::channel::Sender<paths::FileInfo>,
    not_remote_tx: crossbeam::channel::Sender<paths::FileInfo>,
) -> Result<()> {
    for url in url_rx.iter() {
        let file_info = pu_url_parser(&url);
        pu_existence_checker(&file_info, download_tx.clone(), not_remote_tx.clone()).await?;
    }
    Ok(())
}

// ============================================================================
// NOT-FOUND LOGGING STAGE
// Logs file entries that don't exist remotely
// ============================================================================
async fn stage_not_found_logging(not_remote_rx: Receiver<paths::FileInfo>) -> Result<()> {
    for file_info in not_remote_rx.iter() {
        pu_not_found_logger(&file_info);
    }
    Ok(())
}

// ============================================================================
// DOWNLOAD STAGE
// Downloads files from remote URL to local path
// ============================================================================
async fn stage_downloader(download_rx: Receiver<paths::FileInfo>) -> Result<()> {
    for file_info in download_rx.iter() {
        pu_downloader(&file_info).await?;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {

    let filename = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("Usage: binance-downloader <path_to_file_with_urls>, e.g. ./data/data.txt. '-' for <stdio>.");
        std::process::exit(1);
    });

    // Create pipeline channels
    let (url_tx, url_rx) = bounded(PIPE_SIZE);
    let (download_tx, download_rx) = bounded(PIPE_SIZE);
    let (not_remote_tx, not_remote_rx) = bounded(PIPE_SIZE);

    // Spawn file reader stage
    let file_reader_handle = {
        let filename = filename.clone();
        tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Handle::current();
            rt.block_on(stage_file_reader(filename, url_tx))
        })
    };

    // Spawn existence & routing stage
    let existence_router_handle = tokio::spawn(stage_existence_and_route(
        url_rx,
        download_tx,
        not_remote_tx,
    ));

    // Spawn not-found logging stage
    let not_found_logger_handle = tokio::spawn(stage_not_found_logging(not_remote_rx));

    // Spawn download stage
    let downloader_handle = tokio::spawn(stage_downloader(download_rx));

    // Wait for all stages to complete
    let _ = file_reader_handle.await?;
    let _ = existence_router_handle.await?;
    let _ = not_found_logger_handle.await?;
    let _ = downloader_handle.await?;

    Ok(())
}
