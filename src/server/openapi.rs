use serde_json::{json, Value};
use utoipa::{Modify, OpenApi};

#[derive(OpenApi)]
#[openapi(
    paths(
        crate::server::routes::file_parse,
        crate::server::routes::submit_task,
        crate::server::routes::get_task_status,
        crate::server::routes::get_task_result,
        crate::server::routes::health
    ),
    components(schemas(
        crate::domain::models::StatusPayload,
        crate::domain::models::HealthPayload,
        crate::domain::models::TaskStatus
    )),
    modifiers(&MultipartSchema),
    tags((name = "mineru", description = "MinerU-compatible vlm-http-client API"))
)]
pub struct ApiDoc;

pub struct MultipartSchema;

impl Modify for MultipartSchema {
    /// Add MinerU-compatible multipart form schemas for clients generated from OpenAPI.
    ///
    /// Inputs:
    /// - `openapi`: generated OpenAPI document before it is served.
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        set_multipart_schema(openapi, "/file_parse");
        set_multipart_schema(openapi, "/tasks");
    }
}

fn set_multipart_schema(openapi: &mut utoipa::openapi::OpenApi, path: &str) {
    let Some(operation) = openapi
        .paths
        .paths
        .get_mut(path)
        .and_then(|path_item| path_item.post.as_mut())
    else {
        return;
    };
    operation.request_body = serde_json::from_value(json!({
            "required": true,
            "content": {
                "multipart/form-data": {
                    "schema": {
                        "type": "object",
                        "required": ["files"],
                        "properties": multipart_form_properties()
                    },
                    "encoding": {
                        "files": { "style": "form", "explode": true },
                        "lang_list": { "style": "form", "explode": true }
                    }
                }
            }
        }))
        .ok();
}

fn multipart_form_properties() -> Value {
    json!({
        "files": {
            "type": "array",
            "items": { "type": "string", "format": "binary" },
            "description": "Upload PDF or image files for parsing"
        },
        "lang_list": {
            "type": "array",
            "items": { "type": "string" },
            "default": ["ch"]
        },
        "backend": {
            "type": "string",
            "default": "vlm-http-client",
            "enum": ["vlm-http-client", "vllm-http-client"]
        },
        "parse_method": {
            "type": "string",
            "default": "auto",
            "enum": ["auto", "txt", "ocr"]
        },
        "formula_enable": { "type": "boolean", "default": true },
        "table_enable": { "type": "boolean", "default": true },
        "image_analysis": { "type": "boolean", "default": true },
        "server_url": {
            "type": "string",
            "nullable": true,
            "description": "OpenAI-compatible VLM server URL"
        },
        "return_md": { "type": "boolean", "default": true },
        "return_middle_json": { "type": "boolean", "default": false },
        "return_model_output": { "type": "boolean", "default": false },
        "return_content_list": { "type": "boolean", "default": false },
        "return_images": { "type": "boolean", "default": false },
        "response_format_zip": { "type": "boolean", "default": false },
        "return_original_file": { "type": "boolean", "default": false },
        "start_page_id": { "type": "integer", "default": 0, "minimum": 0 },
        "end_page_id": { "type": "integer", "default": 99999, "minimum": 0 }
    })
}
