use std::{
    path::{Path, PathBuf},
    time::Instant,
};

use tokio::fs;

use crate::{
    domain::models::{DocumentKind, ParsedDocument},
    error::ApiResult,
};

pub struct DocumentOutputWriter;

impl DocumentOutputWriter {
    /// Persist one parsed document using MinerU-compatible result filenames.
    ///
    /// Inputs:
    /// - `output_dir`: task output directory.
    /// - `document`: parsed document payload and media files.
    /// - `kind`: document kind used to choose the output subdirectory.
    pub async fn write_document(
        output_dir: &Path,
        document: &ParsedDocument,
        kind: DocumentKind,
    ) -> ApiResult<()> {
        let started_at = Instant::now();
        let parse_dir = Self::parse_dir(output_dir, &document.file_name, kind);
        let images_dir = parse_dir.join("images");
        fs::create_dir_all(&images_dir).await?;
        fs::write(
            parse_dir.join(format!("{}.md", document.file_name)),
            &document.markdown,
        )
        .await?;
        fs::write(
            parse_dir.join(format!("{}_middle.json", document.file_name)),
            serde_json::to_vec_pretty(&document.middle_json)?,
        )
        .await?;
        fs::write(
            parse_dir.join(format!("{}_model.json", document.file_name)),
            serde_json::to_vec_pretty(&document.model_output)?,
        )
        .await?;
        fs::write(
            parse_dir.join(format!("{}_content_list.json", document.file_name)),
            serde_json::to_vec_pretty(&document.content_list)?,
        )
        .await?;
        fs::write(
            parse_dir.join(format!("{}_content_list_v2.json", document.file_name)),
            serde_json::to_vec_pretty(&document.content_list_v2)?,
        )
        .await?;

        for image_file in &document.image_files {
            if let Some(name) = image_file.file_name() {
                fs::copy(image_file, images_dir.join(name)).await?;
            }
        }
        tracing::debug!(
            file_name = %document.file_name,
            output_subdir = kind.output_subdir(),
            image_count = document.image_files.len(),
            elapsed_ms = started_at.elapsed().as_millis(),
            "document files written"
        );
        Ok(())
    }

    pub fn parse_dir(output_dir: &Path, file_name: &str, kind: DocumentKind) -> PathBuf {
        output_dir.join(file_name).join(kind.output_subdir())
    }
}
