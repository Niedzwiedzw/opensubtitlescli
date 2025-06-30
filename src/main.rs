use clap::Parser;
#[allow(unused_imports)]
use eyre::{bail, eyre, Result, WrapErr};
use itertools::Itertools;
use reqwest::Url;
use std::path::{Path, PathBuf};
use std::{
    fs::{self, File},
    io::{BufReader, Read, Seek, SeekFrom},
};
use tap::prelude::*;
use tokio::process::Command;
#[allow(unused_imports)]
use tracing::{debug, error, info, instrument, trace, warn};

const HASH_BLK_SIZE: u64 = 65536;

/// this automates subtitle search
#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// file path
    pub movie_file: PathBuf,
    #[arg(short, long, default_value = "eng")]
    pub language: String,
    /// you will be presented with top n values to choose from
    #[arg(short, long, default_value_t = 1)]
    pub top_n: usize,
}

fn create_hash(file: File, fsize: u64) -> Result<String> {
    let mut buf = [0u8; 8];
    let mut word: u64;

    let mut hash_val: u64 = fsize; // seed hash with file size

    let iterations = HASH_BLK_SIZE / 8;

    let mut reader = BufReader::with_capacity(HASH_BLK_SIZE as usize, file);

    for _ in 0..iterations {
        reader.read_exact(&mut buf)?;
        word = u64::from_ne_bytes(buf);
        hash_val = hash_val.wrapping_add(word);
    }

    reader.seek(SeekFrom::Start(fsize - HASH_BLK_SIZE))?;

    for _ in 0..iterations {
        reader.read_exact(&mut buf)?;
        word = u64::from_ne_bytes(buf);
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

fn url(lang: &str, hash: String) -> Result<Url> {
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
    use ordered_float::OrderedFloat;
    use scraper::{ElementRef, Html, Selector};

    impl std::fmt::Display for SubsEntry {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "[{} (rating: {})]", self.download_url, self.rating)
        }
    }
    #[derive(Debug, Clone)]
    pub struct SubsEntry {
        pub name: String,
        pub flag: String,
        pub cd: String,
        pub sent: String,
        pub download_url: Url,
        pub rating: f32,
        pub edits: i32,
        pub imdb_rating: f32,
        pub uploaded_by: String,
    }

    impl SubsEntry {
        fn from_table_row_element(element: ElementRef<'_>) -> Result<Self> {
            let tr_selector = Selector::parse("td").map_err(|e| eyre!("{e:?}"))?;
            let a_selector = Selector::parse("a").map_err(|e| eyre!("{e:?}"))?;
            let mut trs = element.select(&tr_selector);
            let mut idx: i32 = -1;
            let mut next = || {
                idx += 1;
                trs.next()
                    .ok_or_else(|| eyre!("fetching entry number [{idx}]"))
            };
            Ok(Self {
                name: next().map(|v| v.text().join(" "))?,
                flag: next().map(|v| v.text().join(" "))?,
                cd: next().map(|v| v.text().join(" "))?,
                sent: next().map(|v| v.text().join(" "))?,
                download_url: next().and_then(|tr| {
                    tr.select(&a_selector)
                        .next()
                        .ok_or_else(|| eyre!("no a element"))
                        .and_then(|v| {
                            v.value()
                                .attr("href")
                                .ok_or_else(|| eyre!("no href element"))
                                .and_then(to_url_in_base)
                        })
                        .wrap_err_with(|| format!("extracting download url from [{}]", tr.html()))
                })?,
                rating: next()
                    .and_then(|v| v.text().join(" ").trim().parse().wrap_err("not a float"))?,
                edits: next()
                    .and_then(|v| v.text().join(" ").trim().parse().wrap_err("not an int"))?,
                imdb_rating: next()
                    .and_then(|v| v.text().join(" ").trim().parse().wrap_err("not a float"))?,
                uploaded_by: next().map(|v| v.text().join(" "))?,
            })
        }
    }

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
    pub fn top_rated_subs(page: String, top_n: usize) -> Result<Vec<SubsEntry>> {
        let html = Html::parse_document(&page);
        let tr_selector = Selector::parse("tr").map_err(|e| eyre!("{e:?}"))?;
        let search_results_selector =
            Selector::parse("table#search_results").map_err(|e| eyre!("{e:?}"))?;
        html.select(&search_results_selector)
            .next()
            .ok_or_else(|| eyre!("no search result table"))
            .map(|html| {
                html.select(&tr_selector)
                    .skip(1)
                    .filter_map(|tr| {
                        SubsEntry::from_table_row_element(tr)
                            .wrap_err_with(|| format!("parsing tr:\n{}", tr.html()))
                            .tap_err(|message| {
                                warn!(?message, "parsing failed");
                            })
                            .ok()
                    })
                    .sorted_unstable_by_key(|v| OrderedFloat(-v.rating))
                    .take(top_n)
                    .collect::<Vec<_>>()
            })
    }

    pub fn sub_download_url(page: String) -> Result<Url> {
        let html = Html::parse_document(&page);
        let selector = Selector::parse("tr").map_err(|e| eyre!("{e:?}"))?;

        html.select(&selector)
            .next()
            .ok_or_else(|| eyre!("no element on page"))
            .and_then(|v| {
                v.value()
                    .attr("href")
                    .ok_or_else(|| eyre!("no link present"))
                    .and_then(to_url_in_base)
            })
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

fn prompt_unless_single<T: Clone + std::fmt::Display>(prompt: &str, values: Vec<T>) -> Result<T> {
    match &values[..] {
        [single] => Ok(single.clone()),
        values => inquire::Select::new(prompt, values.to_vec())
            .prompt()
            .wrap_err("invalid selection"),
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let Cli {
        movie_file,
        language,
        top_n,
    } = Cli::parse();
    info!(?movie_file, %language, "downloading");
    let hash = hash_for_file(&movie_file)?;
    info!("hash: {hash}");
    let url = url(&language, hash)?;
    let page = crawler::get_page(url).await?;
    let link = crawler::top_rated_subs(page, top_n).and_then(|values| {
        prompt_unless_single("which url do your want to download", values)
            .wrap_err("selecting url to download")
    })?;
    let download_url = link.download_url;
    let zip = crawler::get_zip(download_url).await?;
    let mut zip_contents = std::io::Cursor::new(zip);
    let mut zip_reader = ::zip::ZipArchive::new(&mut zip_contents).wrap_err("reading zip")?;
    let files = zip_reader
        .file_names()
        .filter(|e| !e.to_lowercase().trim().ends_with(".nfo"))
        .map(|v| v.to_string())
        .sorted_unstable_by_key(|v| v.to_lowercase().ends_with(".srt"))
        .rev()
        .collect::<Vec<_>>();
    info!(?files, "found files");

    let file = prompt_unless_single("Select the subtitle file", files)
        .wrap_err("choosing subtitle file")?;

    let extension = file
        .split('.')
        .next_back()
        .ok_or_else(|| eyre!("this file has no extension"))?;

    let file = zip_reader
        .by_name(&file)
        .wrap_err_with(|| format!("extracting {file} from the archive"))?
        .pipe(BufReader::new)
        .bytes()
        .map(|v| v.wrap_err("invalid byte"))
        .collect::<Result<Vec<_>>>()?;
    let subtitle_file = movie_file.with_extension(extension);
    tokio::fs::write(&subtitle_file, &file)
        .await
        .wrap_err_with(|| format!("writing subtitle file to {subtitle_file:?}"))?;
    println!("{subtitle_file:?}");
    let with_subtitles_name = movie_file
        .extension()
        .and_then(|e| e.to_str())
        .ok_or_else(|| eyre!("file has no extension"))
        .map(|extension| format!("with-subs.{extension}"))
        .map(|extension| movie_file.with_extension(extension))
        .wrap_err_with(|| format!("generating a with-subs file name for [{movie_file:?}]"))?;

    match inquire::Select::new(
        &format!("soft-embed subtitles into [{with_subtitles_name:?}]?"),
        vec![true, false],
    )
    .prompt()
    .unwrap_or_default()
    {
        true => {
            info!(?with_subtitles_name, "saving video with subs to new path");
            Command::new("ffmpeg")
                .arg("-i")
                .arg(movie_file.as_os_str())
                .arg("-i")
                .arg(subtitle_file.as_os_str())
                .args([
                    "-map",
                    "0",
                    "-map",
                    "1",
                    "-c",
                    "copy",
                    "-c:s",
                    "mov_text",
                    "-metadata:s:s:1",
                ])
                .arg(format!("language={language}"))
                .arg(&with_subtitles_name)
                .status()
                .await
                .wrap_err("embedding the subtitles")
                .and_then(|status| {
                    status
                        .success()
                        .then_some(())
                        .ok_or_else(|| eyre!("bad status code: [{status:?}]"))
                })
                .tap_ok(move |_| {
                    info!("file with subtitles available at {with_subtitles_name:?}");
                })
        }
        false => Ok(()),
    }
}
