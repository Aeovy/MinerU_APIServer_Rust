use std::{
    io::{Cursor, Write},
    path::{Path, PathBuf},
};

use axum::{
    body::Body,
    http::{header, Response, StatusCode},
    response::IntoResponse,
    Json,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Map, Value};
use tokio::fs;
use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

use crate::{
    config::MINERU_VERSION,
    domain::models::ParseTask,
    error::{ApiError, ApiResult},
};

pub const FILE_PARSE_TASK_ID_HEADER: &str = "X-MinerU-Task-Id";
pub const FILE_PARSE_TASK_STATUS_HEADER: &str = "X-MinerU-Task-Status";
pub const FILE_PARSE_TASK_STATUS_URL_HEADER: &str = "X-MinerU-Task-Status-Url";
pub const FILE_PARSE_TASK_RESULT_URL_HEADER: &str = "X-MinerU-Task-Result-Url";

pub struct ResultBuilder;

impl ResultBuilder {
    /// Build a JSON or ZIP HTTP response for a completed parse task.
    ///
    /// Inputs:
    /// - `task`: completed task metadata and return flags.
    /// - `status_code`: HTTP response status to use.
    /// - `zip_filename`: download filename when ZIP output is requested.
    pub async fn build_response(
        task: &ParseTask,
        status_code: StatusCode,
        zip_filename: &str,
    ) -> ApiResult<Response<Body>> {
        if task.response_format_zip {
            let bytes = build_zip(task).await?;
            let response = Response::builder()
                .status(status_code)
                .header(header::CONTENT_TYPE, "application/zip")
                .header(
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{zip_filename}\""),
                )
                .body(Body::from(bytes))
                .map_err(|error| ApiError::Internal(error.to_string()))?;
            return Ok(response);
        }

        let payload = Self::build_json_payload(task).await?;
        Ok((status_code, Json(payload)).into_response())
    }

    /// Build the standard MinerU JSON result payload for a completed task.
    ///
    /// Inputs:
    /// - `task`: completed task metadata and return flags.
    pub async fn build_json_payload(task: &ParseTask) -> ApiResult<Value> {
        let results = build_result_dict(task).await?;
        Ok(json!({
            "backend": task.backend,
            "version": MINERU_VERSION,
            "results": results
        }))
    }
}

async fn build_result_dict(task: &ParseTask) -> ApiResult<Value> {
    let mut results = Map::new();
    for file_name in &task.file_names {
        let parse_dir = task.output_dir.join(file_name).join("vlm");
        let mut data = Map::new();
        if task.return_md {
            data.insert(
                "md_content".to_string(),
                read_text(parse_dir.join(format!("{file_name}.md"))).await?,
            );
        }
        if task.return_middle_json {
            data.insert(
                "middle_json".to_string(),
                read_text(parse_dir.join(format!("{file_name}_middle.json"))).await?,
            );
        }
        if task.return_model_output {
            data.insert(
                "model_output".to_string(),
                read_text(parse_dir.join(format!("{file_name}_model.json"))).await?,
            );
        }
        if task.return_content_list {
            data.insert(
                "content_list".to_string(),
                read_text(parse_dir.join(format!("{file_name}_content_list.json"))).await?,
            );
        }
        if task.return_images {
            data.insert(
                "images".to_string(),
                read_images(&parse_dir.join("images")).await?,
            );
        }
        results.insert(file_name.clone(), Value::Object(data));
    }
    Ok(Value::Object(results))
}

async fn build_zip(task: &ParseTask) -> ApiResult<Vec<u8>> {
    let task = task.clone();
    tokio::task::spawn_blocking(move || -> ApiResult<Vec<u8>> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for (file_index, file_name) in task.file_names.iter().enumerate() {
            let parse_dir = task.output_dir.join(file_name).join("vlm");
            add_if_requested(
                &mut writer,
                options,
                task.return_md,
                file_name,
                &parse_dir,
                &format!("{file_name}.md"),
            )?;
            add_if_requested(
                &mut writer,
                options,
                task.return_middle_json,
                file_name,
                &parse_dir,
                &format!("{file_name}_middle.json"),
            )?;
            add_if_requested(
                &mut writer,
                options,
                task.return_model_output,
                file_name,
                &parse_dir,
                &format!("{file_name}_model.json"),
            )?;
            add_if_requested(
                &mut writer,
                options,
                task.return_content_list,
                file_name,
                &parse_dir,
                &format!("{file_name}_content_list.json"),
            )?;
            add_if_requested(
                &mut writer,
                options,
                task.return_content_list,
                file_name,
                &parse_dir,
                &format!("{file_name}_content_list_v2.json"),
            )?;
            if task.return_images {
                let images_dir = parse_dir.join("images");
                if images_dir.is_dir() {
                    for entry in std::fs::read_dir(&images_dir).map_err(ApiError::from)? {
                        let entry = entry.map_err(ApiError::from)?;
                        let path = entry.path();
                        if path.is_file() {
                            let relative = format!(
                                "images/{}",
                                path.file_name()
                                    .and_then(|name| name.to_str())
                                    .unwrap_or("image")
                            );
                            add_file(&mut writer, options, file_name, &parse_dir, &relative)?;
                        }
                    }
                }
            }
            if task.return_original_file {
                if let (Some(upload_path), Some(suffix)) = (
                    task.uploads.get(file_index),
                    task.upload_suffixes.get(file_index),
                ) {
                    let expected_name = format!("{file_name}_origin.{suffix}");
                    let arcname = format!("{file_name}/vlm/{expected_name}");
                    let bytes = std::fs::read(upload_path).map_err(ApiError::from)?;
                    writer
                        .start_file(arcname, options)
                        .map_err(|error| ApiError::Internal(error.to_string()))?;
                    writer.write_all(&bytes).map_err(ApiError::from)?;
                }
            }
        }
        let cursor = writer
            .finish()
            .map_err(|error| ApiError::Internal(error.to_string()))?;
        Ok(cursor.into_inner())
    })
    .await
    .map_err(|error| ApiError::Internal(error.to_string()))?
}

fn add_if_requested(
    writer: &mut ZipWriter<Cursor<Vec<u8>>>,
    options: SimpleFileOptions,
    requested: bool,
    file_name: &str,
    parse_dir: &Path,
    relative_path: &str,
) -> ApiResult<()> {
    if requested {
        add_file(writer, options, file_name, parse_dir, relative_path)?;
    }
    Ok(())
}

fn add_file(
    writer: &mut ZipWriter<Cursor<Vec<u8>>>,
    options: SimpleFileOptions,
    file_name: &str,
    parse_dir: &Path,
    relative_path: &str,
) -> ApiResult<()> {
    let path = parse_dir.join(relative_path);
    if !path.exists() {
        return Ok(());
    }
    let bytes = std::fs::read(&path).map_err(ApiError::from)?;
    let arcname = format!("{file_name}/vlm/{relative_path}");
    writer
        .start_file(arcname, options)
        .map_err(|error| ApiError::Internal(error.to_string()))?;
    writer.write_all(&bytes).map_err(ApiError::from)?;
    Ok(())
}

async fn read_text(path: PathBuf) -> ApiResult<Value> {
    if !path.exists() {
        return Ok(Value::Null);
    }
    Ok(Value::String(fs::read_to_string(path).await?))
}

async fn read_images(images_dir: &Path) -> ApiResult<Value> {
    let mut images = Map::new();
    if !images_dir.is_dir() {
        return Ok(Value::Object(images));
    }
    let mut entries = fs::read_dir(images_dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("image")
            .to_string();
        let mime = mime_guess::from_path(&path)
            .first_raw()
            .unwrap_or("image/jpeg");
        let bytes = fs::read(path).await?;
        images.insert(
            name,
            Value::String(format!("data:{mime};base64,{}", STANDARD.encode(bytes))),
        );
    }
    Ok(Value::Object(images))
}
