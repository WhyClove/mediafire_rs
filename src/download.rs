use crate::api::folder::get_content;
use crate::global::*;
use crate::types::client::Client;
use crate::types::download::DownloadJob;
use crate::types::file::File;
use crate::types::folder::Folder;
use crate::types::get_content::Response;
use crate::utils::check_hash;
use crate::utils::{create_directory_if_not_exists, parse_download_link};
use anyhow::{Result, anyhow};
use futures::StreamExt;
use indicatif::ProgressBar;
use reqwest::header::{HeaderMap, RANGE};
use std::io::SeekFrom;
use std::path::PathBuf;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::try_join;

#[async_recursion::async_recursion]
pub async fn download_folder(
    client: &Client,
    folder_key: &str,
    path: PathBuf,
    chunk: u32,
) -> Result<()> {
    create_directory_if_not_exists(&path).await?;
    TOTAL_PROGRESS_BAR.set_message(format!(
        "{}",
        path.components()
            .last()
            .unwrap()
            .as_os_str()
            .to_str()
            .unwrap()
    ));

    let (folder_content, file_content) =
        get_folder_and_file_content(client, folder_key, chunk).await?;

    if let Some(files) = file_content.folder_content.files {
        download_files(files, &path).await?;
    }

    if let Some(folders) = folder_content.folder_content.folders {
        download_folders(client, folders, &path, chunk).await?;
    }

    if folder_content.folder_content.more_chunks == "yes"
        || file_content.folder_content.more_chunks == "yes"
    {
        download_folder(client, folder_key, path, chunk + 1).await?;
    }

    Ok(())
}

async fn get_folder_and_file_content(
    client: &Client,
    folder_key: &str,
    chunk: u32,
) -> Result<(Response, Response)> {
    match try_join!(
        get_content(client, folder_key, "folders", chunk),
        get_content(client, folder_key, "files", chunk)
    ) {
        Ok((folder_content, file_content)) => Ok((folder_content, file_content)),
        Err(_) => Err(anyhow!("Invalid Mediafire URL")),
    }
}

async fn download_files(files: Vec<File>, path: &PathBuf) -> Result<()> {
    files.iter().for_each(|file| {
        let file_path = path.join(&file.filename);
        let download_job = DownloadJob::new(file.clone(), file_path);
        QUEUE.push(download_job);
    });
    Ok(())
}

async fn download_folders(
    client: &Client,
    folders: Vec<Folder>,
    path: &PathBuf,
    chunk: u32,
) -> Result<()> {
    for folder in folders {
        let folder_path = path.join(&folder.name);
        if let Err(e) = download_folder(client, &folder.folderkey, folder_path, chunk).await {
            return Err(e);
        }
    }
    Ok(())
}

pub async fn download_file(client: &Client, download_job: &DownloadJob) -> Result<()> {
    let bar = MULTI_PROGRESS_BAR.insert_from_back(1, ProgressBar::new(0));
    bar.set_style(PROGRESS_STYLE.clone());
    bar.set_prefix(
        download_job
            .path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string(),
    );

    let mut download_again = false;
    let mut start_bytes = 0;
    if download_job.path.exists() {
        bar.set_message("💾");
        bar.set_prefix("File already exists, checking hash...");
        if check_hash(&download_job.path, &download_job.file.hash)? {
            bar.set_prefix(
                download_job
                    .path
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_string(),
            );
            bar.abandon_with_message("✅");
            return Ok(());
        }
        download_again = true;
        let mut f = tokio::fs::File::open(&download_job.path).await?;
        start_bytes = f.seek(SeekFrom::End(0)).await?;
    }

    bar.set_message("🌀");
    bar.set_prefix(if download_again {
        "Downloading again..."
    } else {
        "Getting download link..."
    });

    let mut headers = HeaderMap::new();
    headers.insert(RANGE, format!("bytes={}-", start_bytes).parse()?);

    let response = {
        let response = client
            .api_client
            .get(&download_job.file.links.normal_download)
            .headers(headers.clone())
            .send()
            .await?;
        if response.headers().get("content-type").unwrap() == &"text/html; charset=UTF-8" {
            if let Some(link) = parse_download_link(&response.text().await?) {
                Some(
                    client
                        .download_client
                        .get(link)
                        .headers(headers)
                        .send()
                        .await?,
                )
            } else {
                None
            }
        } else {
            Some(response)
        }
    };

    bar.set_prefix(
        download_job
            .path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string(),
    );

    if response.is_none() {
        bar.set_style(PROGRESS_STYLE_ERROR.clone());
        bar.abandon_with_message("❌");
        return Err(anyhow!("Error getting download link"));
    }

    if let Some(response) = response {
        if let Err(e) = stream_file_to_disk(&download_job.path, response, &bar).await {
            bar.set_style(PROGRESS_STYLE_ERROR.clone());
            bar.abandon_with_message("❌");
            return Err(e);
        }
        bar.set_message("💾");
        bar.set_prefix("Checking hash...");
        if check_hash(&download_job.path, &download_job.file.hash)? {
            bar.set_prefix(
                download_job
                    .path
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_string(),
            );
            bar.set_style(PROGRESS_STYLE.clone());
            bar.abandon_with_message("✅");
            return Ok(());
        }
    }
    Ok(())
}

pub async fn stream_file_to_disk(
    path: &PathBuf,
    response: reqwest::Response,
    progress_bar: &ProgressBar,
) -> Result<(), anyhow::Error> {
    progress_bar.set_style(PROGRESS_STYLE_DOWNLOAD.clone());
    progress_bar.set_message("🔽");
    let mut file = match path.exists() {
        false => {
            progress_bar.set_length(response.content_length().unwrap());
            tokio::fs::File::create(path).await?
        }
        _ => {
            let mut f = tokio::fs::OpenOptions::new()
                .write(true)
                .append(true)
                .open(path)
                .await?;
            let start_bytes = f.seek(SeekFrom::End(0)).await?;
            progress_bar.set_length(response.content_length().unwrap() + start_bytes);
            progress_bar.inc(start_bytes);
            f
        }
    };

    let mut stream = response.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        progress_bar.inc(chunk.len() as u64);
        file.write_all(&chunk).await?;
        file.flush().await?;
    }
    Ok(())
}
