
#![allow(dead_code)]

pub(crate) mod processor;
pub(crate) mod processor_unit;
pub(crate) mod stages;
pub(crate) mod scraper;
pub(crate) mod file_info;
pub(crate) mod pipeline;

use std::fs::File;
use std::io::{BufRead, BufReader, stdin};

use crossbeam::channel::bounded;

use crate::file_info::FileInfo;
use crate::pipeline::{Event, Pipeline};
use crate::scraper::{extract_pairs, extract_zip_files};

const PIPE_SIZE: usize = 20;


pub fn buffered_reader(filename: String) -> anyhow::Result<Box<dyn BufRead + Send>> {
    let reader: Box<dyn BufRead + Send> = if filename == "-" {
        Box::new(BufReader::new(stdin()))
    } else {
        Box::new(BufReader::new(File::open(filename)?))
    };
    Ok(reader)
}

        #[tokio::main]
async fn main() -> anyhow::Result<()> {

    let pipeline = Pipeline {
        url: bounded(PIPE_SIZE).into(),
        download: bounded(PIPE_SIZE).into(),
        not_remote: bounded(PIPE_SIZE).into(),
    };

    let handler = if let Some(filename) = std::env::args().nth(1)  {
        let reader = buffered_reader(filename)?;
        let pipeline_clone = pipeline.clone();

        tokio::task::spawn_blocking(move || {
            for line in reader.lines() {
                let line = line?;
                        pipeline_clone.url.tx.send(Event::Data(line))?;
            }
            pipeline_clone.url.tx.send(Event::End)?;
            Ok::<(), anyhow::Error>(())
        })
    }
    else {
        let pipeline_clone = pipeline.clone();

        tokio::task::spawn(async move {
            let (_, content) = FileInfo::download_html("https://data.binance.vision/?prefix=data/spot/monthly/klines/").await?;
            println!("{}", content.clone());
            let pairs = extract_pairs(&content);
            for pair in pairs.iter() {
                let klines_url = format!(
                    "https://data.binance.vision/data/spot/monthly/klines/{}/1m/",
                    pair
                );
                let (_, klines_content) = FileInfo::download_html(&klines_url).await?;
                let zip_files = extract_zip_files(&klines_content).0;
                for zip_file in zip_files.iter() {
                    // Do something with the pair and zip_files
                    let filename = format!("https://data.binance.vision/data/spot/monthly/klines/{}/1m/{}", pair, zip_file);
                    pipeline_clone.url.tx.send(Event::Data(filename))?;
                }
            }
            pipeline_clone.url.tx.send(Event::End)?;
            Ok::<(), anyhow::Error>(())
        })
    };

    // read from pipeline.url, check existence, route to download or not_remote
    while let Ok(event) = pipeline.url.rx.recv() {
        match event {
            Event::Data(url) => {
                let file_info = crate::processor_unit::url_parser(&url);
                println!("Received URL: {}, parsed as: {:?}", url, file_info);
             //   crate::processor_unit::existence_checker(&file_info, pipeline.download.tx.clone(), pipeline.not_remote.tx.clone()).await?;
            }
            Event::End => break,
        }
    }
    println!("Finished reading URLs. Starting processing...");

    // perform_download(pipeline).await?;

    let _ = handler.await?;

    Ok(())
}

async fn perform_download(pipeline: Pipeline) -> anyhow::Result<()> {

    // Spawn parallel existence & routing stage — up to PIPE_SIZE concurrent checks
    let url_rx = pipeline.url.rx.clone();
    let download_tx = pipeline.download.tx.clone();
    let not_remote_tx = pipeline.not_remote.tx.clone();
    let existence_router_handle = tokio::spawn(async move {
        stages::parallel_existence_and_route(url_rx, download_tx, not_remote_tx, PIPE_SIZE).await
    });

    // Spawn notfound logging stage
    let not_remote_rx = pipeline.not_remote.rx.clone();
    let not_found_logger_handle = tokio::spawn(async move {
        stages::not_found_logging(not_remote_rx).await
    });

    // Spawn parallel download stage — up to PIPE_SIZE concurrent downloads
    let download_rx = pipeline.download.rx.clone();
    let downloader_handle = tokio::spawn(async move {
            stages::parallel_downloader(download_rx, PIPE_SIZE).await
    });

    // Wait for all stages to complete
    let _ = existence_router_handle.await?;
    let _ = not_found_logger_handle.await?;
    let _ = downloader_handle.await?;

    Ok(())
}