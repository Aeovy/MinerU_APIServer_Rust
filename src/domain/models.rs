use std::{env, path::PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct ParseOptions {
    pub lang_list: Vec<String>,
    pub backend: String,
    pub parse_method: String,
    pub formula_enable: bool,
    pub table_enable: bool,
    pub image_analysis: bool,
    pub server_url: Option<String>,
    pub return_md: bool,
    pub return_middle_json: bool,
    pub return_model_output: bool,
    pub return_content_list: bool,
    pub return_images: bool,
    pub response_format_zip: bool,
    pub return_original_file: bool,
    pub start_page_id: usize,
    pub end_page_id: usize,
}

impl Default for ParseOptions {
    fn default() -> Self {
        Self {
            lang_list: vec!["ch".to_string()],
            backend: read_string_env("MINERU_API_DEFAULT_BACKEND", "vlm-http-client"),
            parse_method: "auto".to_string(),
            formula_enable: true,
            table_enable: true,
            image_analysis: true,
            server_url: read_optional_string_env("MINERU_API_DEFAULT_SERVER_URL"),
            return_md: true,
            return_middle_json: false,
            return_model_output: false,
            return_content_list: false,
            return_images: false,
            response_format_zip: false,
            return_original_file: false,
            start_page_id: 0,
            end_page_id: 99999,
        }
    }
}

impl ParseOptions {
    /// Validate options supported by the Rust native VLM HTTP client implementation.
    ///
    /// Inputs:
    /// - `self`: parsed multipart form options.
    pub fn validate(&self) -> Result<(), String> {
        if !matches!(self.parse_method.as_str(), "auto" | "txt" | "ocr") {
            return Err("Invalid parse_method. Allowed values: auto, ocr, txt".to_string());
        }
        if !is_vlm_http_client_backend(&self.backend) {
            return Err(format!(
                "Unsupported backend: {}. Rust native service currently supports vlm-http-client only.",
                self.backend
            ));
        }
        if self.end_page_id < self.start_page_id {
            return Err("end_page_id must be greater than or equal to start_page_id".to_string());
        }
        Ok(())
    }

    pub fn server_url_has_value(&self) -> bool {
        self.server_url
            .as_deref()
            .map(str::trim)
            .is_some_and(|value| !value.is_empty())
    }

    /// Normalize supported backend aliases to the MinerU API backend name.
    ///
    /// Inputs:
    /// - `self`: parsed multipart form options before task creation.
    pub fn normalize_backend_alias(&mut self) {
        if self.backend == "vllm-http-client" {
            self.backend = "vlm-http-client".to_string();
        }
    }
}

fn is_vlm_http_client_backend(backend: &str) -> bool {
    matches!(backend, "vlm-http-client" | "vllm-http-client")
}

fn read_string_env(name: &str, default: &str) -> String {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn read_optional_string_env(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[derive(Debug, Clone)]
pub struct StoredUpload {
    pub stem: String,
    pub path: PathBuf,
    pub suffix: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Pending,
    Processing,
    Completed,
    Failed,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Processing => "processing",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed)
    }
}

#[derive(Debug, Clone)]
pub struct ParseTask {
    pub task_id: Uuid,
    pub status: TaskStatus,
    pub backend: String,
    pub file_names: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub output_dir: PathBuf,
    pub image_analysis: bool,
    pub server_url: Option<String>,
    pub return_md: bool,
    pub return_middle_json: bool,
    pub return_model_output: bool,
    pub return_content_list: bool,
    pub return_images: bool,
    pub response_format_zip: bool,
    pub return_original_file: bool,
    pub start_page_id: usize,
    pub end_page_id: usize,
    pub uploads: Vec<PathBuf>,
    pub upload_suffixes: Vec<String>,
    pub submit_order: u64,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
}

impl ParseTask {
    /// Build a task from validated options and already persisted uploads.
    ///
    /// Inputs:
    /// - `options`: request options copied from multipart fields.
    /// - `uploads`: files saved on local disk.
    /// - `output_dir`: task-specific output directory.
    pub fn new(
        task_id: Uuid,
        options: &ParseOptions,
        uploads: Vec<StoredUpload>,
        output_dir: PathBuf,
    ) -> Self {
        Self {
            task_id,
            status: TaskStatus::Pending,
            backend: options.backend.clone(),
            file_names: uploads.iter().map(|upload| upload.stem.clone()).collect(),
            created_at: Utc::now(),
            output_dir,
            image_analysis: options.image_analysis,
            server_url: options.server_url.clone(),
            return_md: options.return_md,
            return_middle_json: options.return_middle_json,
            return_model_output: options.return_model_output,
            return_content_list: options.return_content_list,
            return_images: options.return_images,
            response_format_zip: options.response_format_zip,
            return_original_file: options.return_original_file && options.response_format_zip,
            start_page_id: options.start_page_id,
            end_page_id: options.end_page_id,
            uploads: uploads.iter().map(|upload| upload.path.clone()).collect(),
            upload_suffixes: uploads.iter().map(|upload| upload.suffix.clone()).collect(),
            submit_order: 0,
            started_at: None,
            completed_at: None,
            error: None,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct StatusPayload {
    pub task_id: String,
    pub status: String,
    pub backend: String,
    pub file_names: Vec<String>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub error: Option<String>,
    pub status_url: String,
    pub result_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queued_ahead: Option<usize>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct HealthPayload {
    pub status: String,
    pub version: String,
    pub protocol_version: u8,
    pub queued_tasks: usize,
    pub processing_tasks: usize,
    pub completed_tasks: usize,
    pub failed_tasks: usize,
    pub max_concurrent_requests: usize,
    pub max_upload_size_bytes: usize,
    pub processing_window_size: usize,
    pub vlm_max_concurrency: usize,
    pub task_retention_seconds: u64,
    pub task_cleanup_interval_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentBlock {
    #[serde(rename = "type")]
    pub block_type: String,
    pub bbox: [f32; 4],
    pub angle: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merge_prev: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct ParsedDocument {
    pub file_name: String,
    pub markdown: String,
    pub middle_json: Value,
    pub model_output: Value,
    pub content_list: Value,
    pub content_list_v2: Value,
    pub image_files: Vec<PathBuf>,
}
