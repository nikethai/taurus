use serde::Serialize;

#[derive(Serialize)]
pub struct FileInfo {
    pub name: String,
    pub size: String,
}
