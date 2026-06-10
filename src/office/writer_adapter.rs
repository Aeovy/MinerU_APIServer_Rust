use std::path::PathBuf;

use tokio::fs;

use crate::{
    domain::models::ParseTask,
    error::{ApiError, ApiResult},
    office::{image_serializer::serialize_office_image, model::OfficeImage},
};

pub struct OfficeMediaWriter {
    directory: PathBuf,
}

impl OfficeMediaWriter {
    pub async fn new(task: &ParseTask, file_name: &str) -> ApiResult<Self> {
        let directory = task.output_dir.join("_office_media").join(file_name);
        fs::create_dir_all(&directory).await.map_err(|error| {
            ApiError::internal_context(
                format!(
                    "Failed to create Office media directory: {}",
                    directory.display()
                ),
                error,
            )
        })?;
        Ok(Self { directory })
    }

    /// Write one extracted Office media part to a task-local temporary file.
    ///
    /// Inputs:
    /// - `suggested_name`: file name derived from the OOXML media part.
    /// - `content_type`: optional OOXML content type for the media part.
    /// - `bytes`: image bytes extracted from the package.
    pub async fn write_image(
        &self,
        suggested_name: &str,
        content_type: Option<&str>,
        bytes: &[u8],
    ) -> ApiResult<OfficeImageWrite> {
        let Some(serialized) = serialize_office_image(suggested_name, content_type, bytes)? else {
            return Ok(OfficeImageWrite::Skipped {
                reason: "unsupported_image_format".to_string(),
                detail: format!("Unsupported Office image format: {suggested_name}"),
            });
        };
        let path = self.unique_path(&serialized.file_name).await;
        fs::write(&path, &serialized.bytes).await.map_err(|error| {
            ApiError::internal_context(
                format!(
                    "Failed to write serialized Office image {} to {}",
                    suggested_name,
                    path.display()
                ),
                error,
            )
        })?;
        let stored_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| ApiError::Internal("Office image path has no filename".to_string()))?
            .to_string();
        let display_path = format!("images/{stored_name}");
        let image = OfficeImage {
            display_path,
            source_path: path,
        };
        Ok(OfficeImageWrite::Written {
            image,
            warning: serialized.warning,
        })
    }

    async fn unique_path(&self, file_name: &str) -> PathBuf {
        let initial = self.directory.join(file_name);
        if !initial.exists() {
            return initial;
        }
        for index in 2.. {
            let candidate = self.directory.join(format!("{index}_{file_name}"));
            if !candidate.exists() {
                return candidate;
            }
        }
        unreachable!("unbounded office media path search must return")
    }
}

#[derive(Debug)]
pub enum OfficeImageWrite {
    Written {
        image: OfficeImage,
        warning: Option<String>,
    },
    Skipped {
        reason: String,
        detail: String,
    },
}
