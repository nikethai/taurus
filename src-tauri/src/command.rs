#[tauri::command]
pub fn greet() {
    println!("I was invoked from JS!");
}

#[tauri::command]
pub async fn get_download_info(link: &str) -> Result<String, String> {
    let client = reqwest::Client::new();
    let resp = client.get(link).send().await.unwrap_or_else(|e| {
        println!("Error: {}", e);
        return Err(e.to_string()).unwrap();
    });
    // let resp_header = resp.headers();
    let resp_name = resp
        .url()
        .path_segments()
        .and_then(|segments| segments.last())
        .and_then(|name| if name.is_empty() { None } else { Some(name) });
    match resp_name {
        Some(name) => Ok(name.to_string()),
        None => Err(anyhow::anyhow!("Cannot get file name").to_string()),
    }
}
