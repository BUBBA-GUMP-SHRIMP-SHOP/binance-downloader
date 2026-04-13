use scrape_core::Soup;

pub fn extract_pairs(html: &str) -> Vec<String> {
    let soup = Soup::parse(html);
    soup.find_all("a[href]")
        .iter()
        .flat_map(|f| f)
        .skip(10)
        .map(|a| a.text().trim().replace("/", "").to_string())
        .collect::<Vec<_>>()
}

pub fn extract_zip_files(html: &str) -> (Vec<String>, Vec<String>) {
    let soup = Soup::parse(html);
    let zip_files = soup
        .find_all("a[href]")
        .iter()
        .flat_map(|f| f)
        .filter(|a| a.text().ends_with("zip"))
        .map(|a| a.text().trim().replace("/", "").to_string())
        .collect::<Vec<_>>();
    let checksum = soup
        .find_all("a[href]")
        .iter()
        .flat_map(|f| f)
        .filter(|a| a.text().ends_with("CHECKSUM"))
        .map(|a| a.text().trim().replace("/", "").to_string())
        .collect::<Vec<_>>();
    (zip_files, checksum)
}

// pub fn feed(url: &str, sender: crossbeam::channel::Sender<FileInfo>) -> anyhow::Result<()> {
//     // load html of original page
//     let html_pairs = read_to_string(url)?;

//     extract_pairs(&html_pairs).iter().for_each(|pair| {
//         // create url

//         // load html of page with zip files
//         let html_zip_files =
//             read_to_string("./data/Binance Data Collection.html").with_context(|| "can't load")?;
//         let (zip_files, _checksum) = extract_zip_files(&html_zip_files);

//         zip_files.iter().for_each(|zip_file| {
//             let url = format!(
//                 "https://data.binance.vision/data/spot/monthly/klines/{}/{}",
//                 pair, zip_file
//             );
//             //sender.send(FileInfo::from(&url)).unwrap();
//         });
//     });

//     Ok(())
// }
