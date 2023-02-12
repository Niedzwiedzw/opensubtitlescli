#[allow(unused_imports)]
use eyre::{bail, eyre, Result, WrapErr};
use reqwest::Url;
use std::fs;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::mem;
#[allow(unused_imports)]
use tracing::{debug, error, info, instrument, trace, warn};

use clap::Parser;
use std::path::{Path, PathBuf};

const HASH_BLK_SIZE: u64 = 65536;

/// this automates subtitle search
#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// file path
    #[arg(short, long)]
    pub movie_file: PathBuf,
    #[arg(short, long, default_value = "eng")]
    pub language: String,
}

fn create_hash(file: File, fsize: u64) -> Result<String> {
    let mut buf = [0u8; 8];
    let mut word: u64;

    let mut hash_val: u64 = fsize; // seed hash with file size

    let iterations = HASH_BLK_SIZE / 8;

    let mut reader = BufReader::with_capacity(HASH_BLK_SIZE as usize, file);

    for _ in 0..iterations {
        reader.read_exact(&mut buf)?;
        unsafe {
            word = mem::transmute(buf);
        };
        hash_val = hash_val.wrapping_add(word);
    }

    reader.seek(SeekFrom::Start(fsize - HASH_BLK_SIZE))?;

    for _ in 0..iterations {
        reader.read_exact(&mut buf)?;
        unsafe {
            word = mem::transmute(buf);
        };
        hash_val = hash_val.wrapping_add(word);
    }

    let hash_string = format!("{:01$x}", hash_val, 16);

    Ok(hash_string)
}

fn hash_for_file<P: AsRef<Path> + std::fmt::Debug>(path: P) -> Result<String> {
    let size = fs::metadata(&path).wrap_err("checking file size")?.len();
    if size <= HASH_BLK_SIZE {
        bail!("file too small");
    }
    create_hash(std::fs::File::open(path).wrap_err("opening file")?, size)
}
static BASE_URL: &str = "https://www.opensubtitles.org";
fn url(lang: String, hash: String) -> Result<Url> {
    format!("{BASE_URL}/pl/search/sublanguageid-{lang}/moviehash-{hash}")
        .parse()
        .wrap_err("invalid url")
}

fn to_url_in_base(url: &str) -> Result<Url> {
    let url = match url.starts_with(BASE_URL) {
        true => url.to_string(),
        false => format!("{BASE_URL}{url}"),
    };
    url.parse().wrap_err_with(|| format!("invalid url: {url}"))
}

pub mod crawler {
    use super::*;
    use scraper::{Html, Selector};

    #[instrument(fields(url=%url))]
    pub async fn get_page(url: Url) -> Result<String> {
        info!("fetching page");
        reqwest::get(url)
            .await
            .wrap_err("fetching")?
            .text()
            .await
            .wrap_err("parsing page string")
    }
    pub fn top_rated_sub(page: String) -> Result<Url> {
        let html = Html::parse_document(&page);
        let selector = Selector::parse("a.bnone").map_err(|e| eyre!("{e:?}"))?;
        let link = html
            .select(&selector)
            .next()
            .ok_or_else(|| eyre!("no elements found"))?;
        link.value()
            .attr("href")
            .ok_or_else(|| eyre!("no link present"))
            .and_then(to_url_in_base)
    }

    pub fn sub_download_url(page: String) -> Result<Url> {
        let html = Html::parse_document(&page);
        let selector = Selector::parse("#bt-dwl-bt").map_err(|e| eyre!("{e:?}"))?;
        let link = html
            .select(&selector)
            .next()
            .ok_or_else(|| eyre!("no elements found"))?;
        link.value()
            .attr("href")
            .ok_or_else(|| eyre!("no link present"))
            .and_then(to_url_in_base)
    }

    pub async fn get_zip(url: Url) -> Result<Vec<u8>> {
        reqwest::get(url)
            .await
            .wrap_err("fetching")?
            .bytes()
            .await
            .wrap_err("parsing page string")
            .map(|v| v.to_vec())
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let Cli {
        movie_file,
        language,
    } = Cli::parse();
    info!(?movie_file, %language, "downloading");
    let hash = hash_for_file(&movie_file)?;
    let url = url(language, hash)?;
    let page = crawler::get_page(url).await?;
    let link = crawler::top_rated_sub(page)?;
    let download_page = crawler::get_page(link).await?;
    let download_url = crawler::sub_download_url(download_page)?;
    let zip = crawler::get_zip(download_url).await?;
    let mut zip_contents = std::io::Cursor::new(zip);
    let mut zip_reader = ::zip::ZipArchive::new(&mut zip_contents).wrap_err("reading zip")?;
    let files = zip_reader
        .file_names()
        .map(|v| v.to_string())
        .collect::<Vec<_>>();
    info!(?files, "found files");
    let file = inquire::Select::new("Select the subtitle file", files)
        .prompt()
        .wrap_err("you must choose a valid subtitle file")?;
    let extension = file
        .split('.')
        .last()
        .ok_or_else(|| eyre!("this file has no extension"))?;

    let file = zip_reader
        .by_name(&file)
        .wrap_err_with(|| format!("extracting {file} from the archive"))?
        .bytes()
        .map(|v| v.wrap_err("invalid byte"))
        .collect::<Result<Vec<_>>>()?;
    let target_file_name = movie_file.with_extension(extension);
    tokio::fs::write(&target_file_name, &file)
        .await
        .wrap_err_with(|| format!("writing subtitle file to {target_file_name:?}"))?;
    println!("{target_file_name:?}");
    Ok(())
}
