use faccess::PathExt;
use reqwest::Client;
use serde_json::{json, Value};
use std::{path::Path, time::Duration};
use tauri::{Manager, Window, Wry};
use tauri_awesome_rpc::AwesomeEmit;
use tokio::{fs as tfs, io::AsyncWriteExt, time::timeout};
use tokio_retry::RetryIf;

use crate::{
    client,
    model::FileInfo,
    retry::{retry_handler, retry_strategy},
    tau_err::TauError,
    user_agent,
};

#[tauri::command]
pub fn greet() {
    println!("I was invoked from JS!");
}

#[tauri::command]
pub async fn get_download_info(link: &str) -> Result<Value, String> {
    let client = client::make_client(Some(user_agent::UserAgent::RandomUserAgent), None);
    let client = match client {
        Ok(client) => client,
        Err(err) => {
            return Err(anyhow::anyhow!("Cannot create reqwest client {err}").to_string());
        }
    };

    let resp = client.get(link).send().await.unwrap_or_else(|e| {
        println!("Error: {}", e);
        return Err(e.to_string()).unwrap();
    });

    let resp_name = resp
        .url()
        .path_segments()
        .and_then(|segments| segments.last())
        .and_then(|name| if name.is_empty() { None } else { Some(name) });

    let file_name = match resp_name {
        Some(name) => Ok(name.to_string()),
        None => Err(anyhow::anyhow!("Cannot get file name").to_string()),
    };

    let cl_resp_header = resp.headers().get("Content-Length");
    let ar_resp_header = resp.headers().get("Accept-Ranges");

    let size = match cl_resp_header {
        Some(header) => {
            let size = header.to_str().unwrap();
            Ok(format!("{}", size))
        }
        None => Err(anyhow::anyhow!("Cannot get file size").to_string()),
    };

    let is_resumable = match ar_resp_header {
        Some(_) => true,
        None => false,
    };

    let file_info = FileInfo {
        name: file_name?,
        size: size?,
        is_resumable,
    };

    Ok(json!(file_info))
}

#[tauri::command]
pub fn cancel_get_download_info() {
    drop(get_download_info);
}

#[tauri::command]
pub async fn rp_time_elapsed(window: Window<Wry>, id: &str) -> Result<(), ()> {
    // 1 sec
    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(1));
    let start_time = std::time::Instant::now();

    let event_name = format!("time_elapsed_{}", id);

    loop {
        interval.tick().await;

        //emit
        window.state::<AwesomeEmit>().emit(
            "main",
            event_name.as_str(),
            json!(start_time.elapsed().as_secs()),
        );
    }
}

#[tauri::command]
pub async fn download_file(
    window: Window<Wry>,
    id: &str,
    link: &str,
    save_path: &str,
    file_name: &str,
    content_length: &str,
) -> Result<(), String> {
    // Create client
    let client = client::make_client(Some(user_agent::UserAgent::RandomUserAgent), None)
        .expect("Cannot create reqwest client {err}");

    // retry downloading file 10 times
    let retry_strat = retry_strategy(10);

    let int_content_length = content_length
        .parse::<u64>()
        .expect("Cannot parse content length");

    let process = RetryIf::spawn(
        retry_strat,
        || {
            download(
                &client,
                &window,
                id,
                link,
                save_path,
                file_name,
                &int_content_length,
            )
        },
        |e: &TauError| retry_handler(e, link),
    )
    .await;

    match process {
        Ok(_) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

async fn download(
    client: &Client,
    window: &Window<Wry>,
    id: &str,
    link: &str,
    save_path: &str,
    file_name: &str,
    content_length: &u64,
) -> Result<String, TauError> {
    let final_file_path = &format!("{}/{}", save_path, file_name);
    let event_name = format!("download_progress_{}", id);

    if Path::new(final_file_path).exists() {
        let msg = format!("File already exists at {}", save_path);
        Ok(msg)
    } else {
        // Heztner close connecttion if head request is sent
        // For some fking reason
        let get_result = client.get(link).send().await?;
        if !get_result.status().is_success() {
            let message = format!("{} {}", link, get_result.status());
            Err(TauError::ResponseStatusNotSuccess { message })
        } else {
            let accept_range = get_result.headers().get("Accept-Ranges");
            let accept_range = match accept_range {
                Some(accept_range) => Some(accept_range.to_str().unwrap()),
                _ => None,
            };

            let tmp_name = format!("{}/{}.tmp", save_path, file_name);

            // query range is for pause and resume download
            let query_range =
                compute_query_range(Some(*content_length), accept_range, &tmp_name).await?;

            // create/open file.part
            // if file.part exists and query range available, resume download
            let mut file = match query_range {
                Some(_) => {
                    tfs::OpenOptions::new()
                        .append(true)
                        .create(false)
                        .open(&tmp_name)
                        .await?
                }
                None => tfs::File::create(&tmp_name).await?,
            };

            // building the request
            let mut request = client.get(link);
            if let Some(range) = query_range {
                request = request.header("Range", range)
            }

            // initiate file download
            let mut dl_response = request.send().await?;
            if !dl_response.status().is_success() {
                let message = format!("{} {}", link, dl_response.status());
                Err(TauError::ResponseStatusNotSuccess { message })
            } else {
                // incremental save chunk by chunk into part file
                // aka chunk every sec (not sure)
                let chunk_timeout = Duration::from_secs(60);
                while let Some(chunk) = timeout(chunk_timeout, dl_response.chunk()).await?? {
                    file.write_all(&chunk).await?;
                    //emit
                    window.state::<AwesomeEmit>().emit(
                        "main",
                        event_name.as_str(),
                        json!({
                           "chunkPerSec": chunk.len()
                        }),
                    );
                }
                // rename part file to final
                tfs::rename(&tmp_name, final_file_path).await?;
                let msg = format!("Completed {}", file_name,);
                Ok(msg)
            }
        }
    }
}

async fn compute_query_range(
    content_length: Option<u64>,
    accept_ranges: Option<&str>,
    tmp_name: &str,
) -> Result<Option<String>, TauError> {
    if Path::new(&tmp_name).exists() {
        // get existing file size
        let tmp_size = tfs::File::open(&tmp_name).await?.metadata().await?.len();
        match (accept_ranges, content_length) {
            (Some(range), Some(cl)) if range.to_string() == "bytes" => {
                // pb_dl.set_position(tmp_size);
                let range_msg = format!("bytes={}-{}", tmp_size, cl);
                Ok(Some(range_msg))
            }
            _ => Ok(None),
        }
    } else {
        Ok(None)
    }
}

#[tauri::command]
pub fn get_disk_info(path: &str) -> Result<u64, String> {
    let available_space = fs2::available_space(path);
    match available_space {
        Ok(space) => Ok(space),
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
pub fn check_permission(path: &str) -> bool {
    let path = Path::new(path);
    path.writable()
}

#[tauri::command]
pub async fn exists(path: String) -> bool {
    Path::new(&path).exists()
}
