use std::{
    path::{Path, PathBuf},
    time::Instant,
};

use tokio::fs;

use crate::{
    domain::models::{DocumentKind, ParsedDocument},
    error::{ApiError, ApiResult},
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
        fs::create_dir_all(&images_dir).await.map_err(|error| {
            ApiError::internal_context(
                format!(
                    "Failed to create document images directory: {}",
                    images_dir.display()
                ),
                error,
            )
        })?;
        write_result_file(
            &parse_dir.join(format!("{}.md", document.file_name)),
            document.markdown.as_bytes(),
        )
        .await?;
        write_result_file(
            &parse_dir.join(format!("{}_middle.json", document.file_name)),
            &serde_json::to_vec_pretty(&document.middle_json)?,
        )
        .await?;
        write_result_file(
            &parse_dir.join(format!("{}_model.json", document.file_name)),
            &serde_json::to_vec_pretty(&document.model_output)?,
        )
        .await?;
        write_result_file(
            &parse_dir.join(format!("{}_content_list.json", document.file_name)),
            &serde_json::to_vec_pretty(&document.content_list)?,
        )
        .await?;
        write_result_file(
            &parse_dir.join(format!("{}_content_list_v2.json", document.file_name)),
            &serde_json::to_vec_pretty(&document.content_list_v2)?,
        )
        .await?;

        for image_file in &document.image_files {
            if let Some(name) = image_file.file_name() {
                let destination = images_dir.join(name);
                fs::copy(image_file, &destination).await.map_err(|error| {
                    ApiError::internal_context(
                        format!(
                            "Failed to copy result image from {} to {}",
                            image_file.display(),
                            destination.display()
                        ),
                        error,
                    )
                })?;
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

async fn write_result_file(path: &Path, bytes: &[u8]) -> ApiResult<()> {
    fs::write(path, bytes).await.map_err(|error| {
        ApiError::internal_context(
            format!("Failed to write result file: {}", path.display()),
            error,
        )
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tempfile::tempdir;

    use crate::{
        domain::models::{DocumentKind, ParsedDocument},
        io::document_writer::DocumentOutputWriter,
    };

    #[tokio::test]
    async fn write_document_error_includes_output_path() {
        let temp = tempdir().expect("tempdir");
        let blocked_root = temp.path().join("blocked");
        tokio::fs::write(&blocked_root, b"not a directory")
            .await
            .expect("blocked file should write");
        let document = ParsedDocument {
            file_name: "sample".to_string(),
            markdown: "# ok\n".to_string(),
            middle_json: json!({}),
            model_output: json!({}),
            content_list: json!([]),
            content_list_v2: json!([]),
            image_files: Vec::new(),
        };

        let error =
            DocumentOutputWriter::write_document(&blocked_root, &document, DocumentKind::Office)
                .await
                .expect_err("blocked output root should fail");

        assert!(error.detail().contains("sample/office/images"));
    }
}
