use crate::{file_info::FileInfo, pipeline::Event};

// ============================================================================
// PROCESSING UNIT: URL Parser
// Converts raw URL strings into FileInfo structures
// ============================================================================
pub fn url_parser(url: &String) -> FileInfo {
    FileInfo::from(url.as_str())
}

// ============================================================================
// PROCESSING UNIT: Existence Checker
// Checks local/remote existence and routes via side effects
// Routes: locally existing -> log only
//         remote exists -> download channel
//         remote missing -> not-remote channel
// ============================================================================
pub async fn existence_checker(
    file_info: &FileInfo,
    download_tx: crossbeam::channel::Sender<Event<FileInfo>>,
    not_remote_tx: crossbeam::channel::Sender<Event<FileInfo>>,
) -> anyhow::Result<()> {
    if file_info.local_path_exists()? {
        println!(
            "🤟 File already exists locally:      {}",
            file_info.local_path()
        );
        return Ok(());
    }

    if file_info.remote_url_exists().await? {
        download_tx.send(Event::Data(file_info.clone()))?;
    } else {
        not_remote_tx.send(Event::Data(file_info.clone()))?;
    }

    Ok(())
}

// ============================================================================
// PROCESSING UNIT: Not Found Logger
// Logs FileInfo entries where remote URL was not found
// ============================================================================
pub fn not_found_logger(file_info: &FileInfo) -> () {
    println!(
        "❌ Remote URL does not exist:        {}",
        file_info.remote_url()
    );
}

// ============================================================================
// PROCESSING UNIT: Downloader
// Downloads files from remote URL to local path
// ============================================================================
pub async fn downloader(file_info: &FileInfo) -> anyhow::Result<()> {
    file_info.download_zip().await?;
    Ok(())
}
