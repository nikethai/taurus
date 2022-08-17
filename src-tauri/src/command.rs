#[tauri::command]
pub fn greet() {
    println!("I was invoked from JS!");
}


#[tauri::command]
pub async fn get_download_info(link: &str) {
    
}