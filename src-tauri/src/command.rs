use serde_json::{json, Value};
use tauri::{Manager, Window, Wry};
use tauri_awesome_rpc::AwesomeEmit;

use crate::model::FileInfo;

#[tauri::command]
pub fn greet() {
    println!("I was invoked from JS!");
}

#[tauri::command]
pub async fn get_download_info(link: &str) -> Result<Value, String> {
    let client = reqwest::Client::new();
    let resp = client.get(link).send().await.unwrap_or_else(|e| {
        println!("Error: {}", e);
        return Err(e.to_string()).unwrap();
    });

    let resp_name = resp
        .url()
        .path_segments()
        .and_then(|segments| segments.last())
        .and_then(|name| if name.is_empty() { None } else { Some(name) });

    let res = match resp_name {
        Some(name) => Ok(name.to_string()),
        None => Err(anyhow::anyhow!("Cannot get file name").to_string()),
    };

    let resp_header = resp.headers().get("Content-Length");

    let size = match resp_header {
        Some(header) => {
            let size = header.to_str().unwrap();
            Ok(format!("{}", size))
        }
        None => Err(anyhow::anyhow!("Cannot get file size").to_string()),
    };

    let file_info = FileInfo {
        name: res?,
        size: size?,
    };

    Ok(json!(file_info)) // return without return syntax sometime ppl will confuse
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
