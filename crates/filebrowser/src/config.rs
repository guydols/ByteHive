use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct FileBrowserConfig {
    pub root: Option<String>,
    pub max_upload_mb: u64,
    pub allow_delete: bool,
}

impl Default for FileBrowserConfig {
    fn default() -> Self {
        Self {
            root: None,
            max_upload_mb: 200,
            allow_delete: true,
        }
    }
}
