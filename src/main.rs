
pub(crate) mod paths;



use std::fs::read_to_string;

use anyhow::Result;
use crossbeam::channel::bounded;
use tokio::fs::File;
use tokio::io::BufReader;


const PIPE_SIZE: usize = 10;


#[tokio::main]
async fn main() -> Result<()> {

    let file = File::open("./data/data.txt")
        .await?;

    let mut file_reader = BufReader::new(file);

    let (mut url_reader, mut url_writer) = bounded(PIPE_SIZE);

    let file_handler = tokio::spawn(async move {
        let mut line = String::new();
        while file_reader.read_line(&mut line).await? > 0 {
            url_writer.send(line.clone()).unwrap();
            line.clear();
        }
    });

    let (mut not_remote_reader, mut not_remote_writer) = bounded(PIPE_SIZE);
    let (mut download_reader, mut download_writer) = bounded(PIPE_SIZE);

    let url_handler = tokio::spawn(async move {
        while let Ok(url) = url_reader.recv() {
            let file_info = paths::FileInfo::from(url.as_str());
            if file_info.remote_url_exists(&file_info.remote_url()).await.unwrap_or(false) {
                download_writer.send(file_info).unwrap();
            } else {
                not_remote_writer.send(file_info).unwrap();
            }
        }
    });

    let not_remote_handler = tokio::spawn(async move {
        while let Ok(file_info) = not_remote_reader.recv() {
            println!("Remote URL does not exist: {}", file_info.remote_url());
        }
    });

    let download_handler = tokio::spawn(async move {
        while let Ok(file_info) = download_reader.recv() {
            println!("Downloading: {}", file_info.remote_url());
            let response = reqwest::get(&file_info.remote_url()).await?;
            let bytes = response.bytes().await?;
            let mut local_file = file_info.create_local_file_for_writing()?;
            tokio::io::copy(&mut bytes.as_ref(), &mut local_file).await?;
        }
    });

    url_handler.await?;
    not_remote_handler.await?;
    download_handler.await?;
    file_handler.await?;

    Ok(())
}






