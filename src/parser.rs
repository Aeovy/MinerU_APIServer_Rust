use std::sync::Arc;
use std::time::Instant;

use crate::{
    domain::models::{DocumentKind, ParseTask, StoredUpload},
    error::{ApiError, ApiResult},
    io::document_writer::DocumentOutputWriter,
    office::OfficeDocumentParser,
    vlm::parser::VlmDocumentParser,
};

#[derive(Clone)]
pub struct DocumentParserRouter {
    vlm_parser: Arc<VlmDocumentParser>,
    office_parser: Arc<OfficeDocumentParser>,
}

impl DocumentParserRouter {
    pub fn new(
        vlm_parser: Arc<VlmDocumentParser>,
        office_parser: Arc<OfficeDocumentParser>,
    ) -> Self {
        Self {
            vlm_parser,
            office_parser,
        }
    }

    /// Parse all uploads in a task and write MinerU-compatible result files.
    ///
    /// Inputs:
    /// - `task`: task options and already persisted uploads.
    pub async fn parse_task(&self, task: &ParseTask) -> ApiResult<Vec<String>> {
        let started_at = Instant::now();
        let mut response_file_names = Vec::new();
        for ((path, stem), suffix) in task
            .uploads
            .iter()
            .zip(task.file_names.iter())
            .zip(task.upload_suffixes.iter())
        {
            let upload = StoredUpload {
                stem: stem.clone(),
                path: path.clone(),
                suffix: suffix.clone(),
            };
            let kind = upload.document_kind().ok_or_else(|| {
                ApiError::BadRequest(format!("Unsupported file type: {}", upload.suffix))
            })?;
            if kind.is_vlm() {
                let mut vlm_task = task.clone();
                vlm_task.file_names = vec![upload.stem.clone()];
                vlm_task.uploads = vec![upload.path.clone()];
                vlm_task.upload_suffixes = vec![upload.suffix.clone()];
                response_file_names.extend(self.vlm_parser.parse_task(&vlm_task).await?);
                continue;
            }

            let upload_started_at = Instant::now();
            let document = self.office_parser.parse_upload(task, &upload).await?;
            let parse_upload_ms = upload_started_at.elapsed().as_millis();
            let write_started_at = Instant::now();
            DocumentOutputWriter::write_document(&task.output_dir, &document, DocumentKind::Office)
                .await?;
            tracing::debug!(
                task_id = %task.task_id,
                file_name = %document.file_name,
                suffix = %upload.suffix,
                parse_upload_ms,
                write_document_ms = write_started_at.elapsed().as_millis(),
                "office document parse output written"
            );
            response_file_names.push(document.file_name);
        }

        tracing::debug!(
            task_id = %task.task_id,
            file_count = response_file_names.len(),
            elapsed_ms = started_at.elapsed().as_millis(),
            "parse task documents completed"
        );
        Ok(response_file_names)
    }
}
