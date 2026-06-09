use std::path::{Path, PathBuf};

use tokio::fs;

use crate::{
    domain::models::ParseTask,
    error::{ApiError, ApiResult},
    office::model::OfficeImage,
};

pub struct OfficeMediaWriter {
    directory: PathBuf,
}

impl OfficeMediaWriter {
    pub async fn new(task: &ParseTask, file_name: &str) -> ApiResult<Self> {
        let directory = task.output_dir.join("_office_media").join(file_name);
        fs::create_dir_all(&directory).await?;
        Ok(Self { directory })
    }

    /// Write one extracted Office media part to a task-local temporary file.
    ///
    /// Inputs:
    /// - `suggested_name`: file name derived from the OOXML media part.
    /// - `bytes`: image bytes extracted from the package.
    pub async fn write_image(&self, suggested_name: &str, bytes: &[u8]) -> ApiResult<OfficeImage> {
        let file_name = sanitize_file_name(suggested_name);
        let path = self.unique_path(&file_name).await;
        fs::write(&path, bytes).await?;
        let stored_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| ApiError::Internal("Office image path has no filename".to_string()))?
            .to_string();
        Ok(OfficeImage {
            file_name: stored_name,
            source_path: path,
        })
    }

    async fn unique_path(&self, file_name: &str) -> PathBuf {
        let initial = self.directory.join(file_name);
        if !initial.exists() {
            return initial;
        }
        let stem = Path::new(file_name)
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("image");
        let extension = Path::new(file_name)
            .extension()
            .and_then(|value| value.to_str())
            .map(|value| format!(".{value}"))
            .unwrap_or_default();
        for index in 2.. {
            let candidate = self.directory.join(format!("{stem}_{index}{extension}"));
            if !candidate.exists() {
                return candidate;
            }
        }
        unreachable!("unbounded office media path search must return")
    }
}

fn sanitize_file_name(value: &str) -> String {
    let file_name = Path::new(value)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("image");
    if file_name.trim().is_empty() {
        return "image".to_string();
    }
    file_name
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            _ => ch,
        })
        .collect()
}
