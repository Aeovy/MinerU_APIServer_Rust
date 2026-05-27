use axum::{
    extract::{DefaultBodyLimit, Multipart, Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::time::Instant;
use tokio::sync::OwnedSemaphorePermit;
use tower_http::{
    compression::{
        predicate::{DefaultPredicate, NotForContentType, Predicate},
        CompressionLayer,
    },
    cors::CorsLayer,
    trace::{DefaultMakeSpan, DefaultOnFailure, DefaultOnRequest, DefaultOnResponse, TraceLayer},
};

use tracing::Level;
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;
use uuid::Uuid;

use crate::{
    config::{API_PROTOCOL_VERSION, MINERU_VERSION},
    domain::models::{HealthPayload, ParseOptions, ParseTask, StoredUpload, TaskStatus},
    error::{ApiError, ApiResult},
    io::{
        result_builder::{
            ResultBuilder, FILE_PARSE_TASK_ID_HEADER, FILE_PARSE_TASK_RESULT_URL_HEADER,
            FILE_PARSE_TASK_STATUS_HEADER, FILE_PARSE_TASK_STATUS_URL_HEADER,
        },
        uploads::{uniquify_upload_stems, UploadStore},
    },
    server::{openapi::ApiDoc, security::validate_public_http_client_policy, state::AppState},
};

pub fn build_router(state: AppState) -> Router {
    let max_upload_size_bytes = state.config().max_upload_size_bytes;
    Router::new()
        .route("/file_parse", post(file_parse))
        .route("/tasks", post(submit_task))
        .route("/tasks/{task_id}", get(get_task_status))
        .route("/tasks/{task_id}/result", get(get_task_result))
        .route("/health", get(health))
        .merge(SwaggerUi::new("/docs").url("/openapi.json", ApiDoc::openapi()))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                .on_request(DefaultOnRequest::new().level(Level::INFO))
                .on_response(DefaultOnResponse::new().level(Level::INFO))
                .on_failure(DefaultOnFailure::new().level(Level::WARN)),
        )
        .layer(CompressionLayer::new().compress_when(
            DefaultPredicate::new().and(NotForContentType::const_new("application/zip")),
        ))
        .layer(DefaultBodyLimit::max(max_upload_size_bytes))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

#[utoipa::path(
    post,
    path = "/file_parse",
    request_body(content_type = "multipart/form-data"),
    responses((status = 200, description = "Synchronously parse uploaded files"))
)]
pub(crate) async fn file_parse(
    State(state): State<AppState>,
    headers: HeaderMap,
    multipart: Multipart,
) -> ApiResult<Response> {
    let started_at = Instant::now();
    let base_url = base_url_from_headers(&headers);
    let admission_permit = acquire_admission_permit(&state).await?;
    let task = create_async_parse_task(&state, multipart).await?;
    let task_manager = state.task_manager();
    let task_id = task.task_id;
    tracing::debug!(
        %task_id,
        file_count = task.file_names.len(),
        response_format_zip = task.response_format_zip,
        return_md = task.return_md,
        return_middle_json = task.return_middle_json,
        return_model_output = task.return_model_output,
        return_content_list = task.return_content_list,
        return_images = task.return_images,
        elapsed_ms = started_at.elapsed().as_millis(),
        "file_parse task created"
    );
    spawn_task_processor(state.clone(), task, admission_permit);
    let wait_started_at = Instant::now();
    let completed = task_manager.wait_terminal(task_id).await.ok_or_else(|| {
        ApiError::ServiceUnavailable("Task was removed before completion".to_string())
    })?;
    tracing::debug!(
        %task_id,
        status = completed.status.as_str(),
        wait_ms = wait_started_at.elapsed().as_millis(),
        total_ms = started_at.elapsed().as_millis(),
        "file_parse task reached terminal status"
    );
    if completed.status == TaskStatus::Failed {
        let status_payload = task_manager.status_payload(&completed, &base_url).await;
        return Ok((
            StatusCode::CONFLICT,
            Json(with_message(status_payload, "Task execution failed")),
        )
            .into_response());
    }

    let status_payload = task_manager.status_payload(&completed, &base_url).await;
    let result_lease = task_manager
        .acquire_result_lease(task_id)
        .await
        .ok_or_else(|| {
            ApiError::ServiceUnavailable("Task was removed before result loading".to_string())
        })?;
    if !completed.response_format_zip {
        let response_started_at = Instant::now();
        let mut payload = serde_json::to_value(&status_payload)?;
        merge_object_fields(
            &mut payload,
            ResultBuilder::build_json_payload(&completed).await?,
        );
        drop(result_lease);
        tracing::debug!(
            %task_id,
            response_format = "json",
            elapsed_ms = response_started_at.elapsed().as_millis(),
            total_ms = started_at.elapsed().as_millis(),
            "file_parse response built"
        );
        return Ok((StatusCode::OK, Json(payload)).into_response());
    }

    let response_started_at = Instant::now();
    let mut response = ResultBuilder::build_response_with_lease(
        &completed,
        StatusCode::OK,
        &format!("{task_id}.zip"),
        Some(result_lease),
    )
    .await?;
    response.headers_mut().insert(
        FILE_PARSE_TASK_ID_HEADER,
        task_id.to_string().parse().expect("uuid header is valid"),
    );
    response.headers_mut().insert(
        FILE_PARSE_TASK_STATUS_HEADER,
        completed
            .status
            .as_str()
            .parse()
            .expect("status header is valid"),
    );
    response.headers_mut().insert(
        FILE_PARSE_TASK_STATUS_URL_HEADER,
        status_payload
            .status_url
            .parse()
            .expect("status url header is valid"),
    );
    response.headers_mut().insert(
        FILE_PARSE_TASK_RESULT_URL_HEADER,
        status_payload
            .result_url
            .parse()
            .expect("result url header is valid"),
    );
    tracing::debug!(
        %task_id,
        response_format = "zip",
        elapsed_ms = response_started_at.elapsed().as_millis(),
        total_ms = started_at.elapsed().as_millis(),
        "file_parse response built"
    );
    Ok(response)
}

#[utoipa::path(
    post,
    path = "/tasks",
    request_body(content_type = "multipart/form-data"),
    responses((status = 202, description = "Submit an asynchronous parse task"))
)]
pub(crate) async fn submit_task(
    State(state): State<AppState>,
    headers: HeaderMap,
    multipart: Multipart,
) -> ApiResult<Response> {
    let base_url = base_url_from_headers(&headers);
    let admission_permit = acquire_admission_permit(&state).await?;
    let task = create_async_parse_task(&state, multipart).await?;
    let task_manager = state.task_manager();
    let payload = task_manager.status_payload(&task, &base_url).await;
    spawn_task_processor(state, task, admission_permit);
    Ok((
        StatusCode::ACCEPTED,
        Json(with_message(payload, "Task submitted successfully")),
    )
        .into_response())
}

#[utoipa::path(
    get,
    path = "/tasks/{task_id}",
    params(("task_id" = String, Path, description = "Task id")),
    responses((status = 200, body = crate::domain::models::StatusPayload))
)]
pub(crate) async fn get_task_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(task_id): Path<Uuid>,
) -> ApiResult<Response> {
    let task_manager = state.task_manager();
    let task = task_manager
        .get(task_id)
        .await
        .ok_or_else(|| ApiError::NotFound("Task not found".to_string()))?;
    let payload = task_manager
        .status_payload(&task, &base_url_from_headers(&headers))
        .await;
    Ok(Json(payload).into_response())
}

#[utoipa::path(
    get,
    path = "/tasks/{task_id}/result",
    params(("task_id" = String, Path, description = "Task id")),
    responses((status = 200, description = "Task result"))
)]
pub(crate) async fn get_task_result(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(task_id): Path<Uuid>,
) -> ApiResult<Response> {
    let task_manager = state.task_manager();
    let task = task_manager
        .get(task_id)
        .await
        .ok_or_else(|| ApiError::NotFound("Task not found".to_string()))?;
    let status_payload = task_manager
        .status_payload(&task, &base_url_from_headers(&headers))
        .await;
    if matches!(task.status, TaskStatus::Pending | TaskStatus::Processing) {
        return Ok((
            StatusCode::ACCEPTED,
            Json(with_message(status_payload, "Task result is not ready yet")),
        )
            .into_response());
    }
    if task.status == TaskStatus::Failed {
        return Ok((
            StatusCode::CONFLICT,
            Json(with_message(status_payload, "Task execution failed")),
        )
            .into_response());
    }
    let result_lease = task_manager
        .acquire_result_lease(task_id)
        .await
        .ok_or_else(|| {
            ApiError::ServiceUnavailable("Task was removed before result loading".to_string())
        })?;
    ResultBuilder::build_response_with_lease(
        &task,
        StatusCode::OK,
        &format!("{task_id}.zip"),
        Some(result_lease),
    )
    .await
}

#[utoipa::path(
    get,
    path = "/health",
    responses((status = 200, body = crate::domain::models::HealthPayload))
)]
pub(crate) async fn health(State(state): State<AppState>) -> ApiResult<Json<HealthPayload>> {
    let stats = state.task_manager().stats().await;
    Ok(Json(HealthPayload {
        status: "healthy".to_string(),
        version: MINERU_VERSION.to_string(),
        protocol_version: API_PROTOCOL_VERSION,
        queued_tasks: stats.pending,
        processing_tasks: stats.processing,
        completed_tasks: stats.completed,
        failed_tasks: stats.failed,
        max_concurrent_requests: state.config().max_concurrent_requests,
        max_in_flight_tasks: state.config().max_in_flight_tasks,
        available_admission_permits: state.available_admission_permits(),
        max_upload_size_bytes: state.config().max_upload_size_bytes,
        processing_window_size: state.config().processing_window_size,
        vlm_max_concurrency: state.config().vlm_max_concurrency,
        available_vlm_permits: state.available_vlm_permits(),
        task_retention_seconds: state.config().task_retention.as_secs(),
        task_cleanup_interval_seconds: state.config().task_cleanup_interval.as_secs(),
    }))
}

async fn acquire_admission_permit(state: &AppState) -> ApiResult<OwnedSemaphorePermit> {
    state
        .admission_semaphore()
        .acquire_owned()
        .await
        .map_err(|error| ApiError::ServiceUnavailable(error.to_string()))
}

async fn create_async_parse_task(state: &AppState, multipart: Multipart) -> ApiResult<ParseTask> {
    let started_at = Instant::now();
    let task_id = Uuid::new_v4();
    let task_output_dir = state.create_task_output_dir(task_id);
    let uploads_dir = task_output_dir.join("uploads");
    tokio::fs::create_dir_all(&uploads_dir).await?;
    let multipart_started_at = Instant::now();
    let (options, mut uploads) = parse_multipart(multipart, UploadStore::new(uploads_dir)).await?;
    let multipart_elapsed_ms = multipart_started_at.elapsed().as_millis();
    validate_public_http_client_policy(
        state.config().public_bind_exposed,
        state.config().allow_public_http_client,
        &options,
    )?;
    options.validate().map_err(ApiError::BadRequest)?;
    if uploads.is_empty() {
        return Err(ApiError::BadRequest("Field required: files".to_string()));
    }
    uniquify_upload_stems(&mut uploads);
    let upload_count = uploads.len();
    let task = ParseTask::new(task_id, &options, uploads, task_output_dir);
    let submitted = state.task_manager().submit(task).await;
    tracing::debug!(
        %task_id,
        upload_count,
        backend = %submitted.backend,
        start_page_id = submitted.start_page_id,
        end_page_id = submitted.end_page_id,
        multipart_ms = multipart_elapsed_ms,
        elapsed_ms = started_at.elapsed().as_millis(),
        "parse task accepted"
    );
    Ok(submitted)
}

async fn parse_multipart(
    mut multipart: Multipart,
    upload_store: UploadStore,
) -> ApiResult<(ParseOptions, Vec<StoredUpload>)> {
    let mut options = ParseOptions::default();
    let mut uploads = Vec::new();
    while let Some(field) = multipart.next_field().await.map_err(ApiError::from)? {
        let name = field.name().unwrap_or_default().to_string();
        if name == "files" {
            uploads.push(upload_store.save_field(field).await?);
            continue;
        }
        let text = field.text().await.map_err(ApiError::from)?;
        apply_form_field(&mut options, &name, &text)?;
    }
    options.return_original_file = options.return_original_file && options.response_format_zip;
    options.normalize_backend_alias();
    Ok((options, uploads))
}

fn apply_form_field(options: &mut ParseOptions, name: &str, value: &str) -> ApiResult<()> {
    match name {
        "lang_list" => options.lang_list.push(value.to_string()),
        "backend" => options.backend = value.to_string(),
        "parse_method" => options.parse_method = value.to_string(),
        "formula_enable" => options.formula_enable = parse_bool(value)?,
        "table_enable" => options.table_enable = parse_bool(value)?,
        "image_analysis" => options.image_analysis = parse_bool(value)?,
        "server_url" => options.server_url = Some(value.to_string()),
        "return_md" => options.return_md = parse_bool(value)?,
        "return_middle_json" => options.return_middle_json = parse_bool(value)?,
        "return_model_output" => options.return_model_output = parse_bool(value)?,
        "return_content_list" => options.return_content_list = parse_bool(value)?,
        "return_images" => options.return_images = parse_bool(value)?,
        "response_format_zip" => options.response_format_zip = parse_bool(value)?,
        "return_original_file" => options.return_original_file = parse_bool(value)?,
        "start_page_id" => options.start_page_id = parse_usize(value, name)?,
        "end_page_id" => options.end_page_id = parse_usize(value, name)?,
        _ => {}
    }
    Ok(())
}

fn parse_bool(value: &str) -> ApiResult<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        other => Err(ApiError::BadRequest(format!(
            "Invalid boolean value: {other}"
        ))),
    }
}

fn parse_usize(value: &str, name: &str) -> ApiResult<usize> {
    value
        .parse::<usize>()
        .map_err(|_| ApiError::BadRequest(format!("Invalid integer value for {name}: {value}")))
}

fn spawn_task_processor(state: AppState, task: ParseTask, admission_permit: OwnedSemaphorePermit) {
    tokio::spawn(async move {
        let _admission_permit = admission_permit;
        let task_id = task.task_id;
        let queue_started_at = Instant::now();
        let permit = match state.request_semaphore().acquire_owned().await {
            Ok(permit) => permit,
            Err(error) => {
                state
                    .task_manager()
                    .set_failed(task_id, error.to_string())
                    .await;
                return;
            }
        };
        tracing::debug!(
            %task_id,
            queue_wait_ms = queue_started_at.elapsed().as_millis(),
            "parse task acquired request permit"
        );
        state.task_manager().set_processing(task_id).await;
        let parse_started_at = Instant::now();
        let result = state.parser().parse_task(&task).await;
        let parse_elapsed_ms = parse_started_at.elapsed().as_millis();
        drop(permit);
        match result {
            Ok(file_names) => {
                tracing::debug!(
                    %task_id,
                    file_count = file_names.len(),
                    parse_ms = parse_elapsed_ms,
                    "parse task completed"
                );
                state
                    .task_manager()
                    .set_completed(task_id, file_names)
                    .await
            }
            Err(error) => {
                tracing::debug!(
                    %task_id,
                    parse_ms = parse_elapsed_ms,
                    error = %error.detail(),
                    "parse task failed"
                );
                state
                    .task_manager()
                    .set_failed(task_id, error.detail())
                    .await
            }
        }
    });
}

fn base_url_from_headers(headers: &HeaderMap) -> String {
    let host = headers
        .get("host")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("127.0.0.1");
    format!("http://{host}")
}

fn with_message(payload: impl serde::Serialize, message: &str) -> Value {
    let mut value = serde_json::to_value(payload).unwrap_or_else(|_| json!({}));
    if let Some(object) = value.as_object_mut() {
        object.insert("message".to_string(), Value::String(message.to_string()));
    }
    value
}

/// Merge JSON object fields from `source` into `target`, preserving Python file_parse response shape.
///
/// Inputs:
/// - `target`: task status payload object to be extended.
/// - `source`: standard result payload object containing backend, version, and results.
fn merge_object_fields(target: &mut Value, source: Value) {
    let Some(target_object) = target.as_object_mut() else {
        return;
    };
    let Value::Object(source_object) = source else {
        return;
    };
    for (key, value) in source_object {
        target_object.insert(key, value);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::{
        config::CliArgs,
        domain::models::ParseOptions,
        server::state::AppState,
        vlm::test_env::{EnvVarGuard, TEST_ENV_LOCK},
    };

    use super::{acquire_admission_permit, apply_form_field, parse_bool};

    #[test]
    fn parses_boolean_values() {
        assert!(parse_bool("true").unwrap());
        assert!(parse_bool("1").unwrap());
        assert!(!parse_bool("false").unwrap());
        assert!(parse_bool("wat").is_err());
    }

    #[test]
    fn applies_form_defaults_and_fields() {
        let mut options = ParseOptions::default();
        apply_form_field(&mut options, "backend", "vlm-http-client").unwrap();
        apply_form_field(&mut options, "return_images", "true").unwrap();
        assert_eq!(options.backend, "vlm-http-client");
        assert!(options.return_images);
    }

    #[test]
    fn normalizes_vllm_http_client_alias() {
        let mut options = ParseOptions::default();
        apply_form_field(&mut options, "backend", "vllm-http-client").unwrap();
        options.normalize_backend_alias();
        assert_eq!(options.backend, "vlm-http-client");
        assert!(options.validate().is_ok());
    }

    #[tokio::test]
    async fn admission_permit_waits_until_capacity_is_released() {
        let _env_lock = TEST_ENV_LOCK.lock().await;
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let _output_root = EnvVarGuard::set(
            "MINERU_API_OUTPUT_ROOT",
            temp.path().to_str().unwrap_or_default(),
        );
        let _in_flight = EnvVarGuard::set("MINERU_API_MAX_IN_FLIGHT_TASKS", "1");
        let state = AppState::new(CliArgs {
            host: "127.0.0.1".to_string(),
            port: 34000,
            allow_public_http_client: false,
        })
        .await
        .expect("state should build");

        let first = acquire_admission_permit(&state)
            .await
            .expect("first permit should acquire");
        assert_eq!(state.available_admission_permits(), 0);

        let waiting = acquire_admission_permit(&state);
        assert!(tokio::time::timeout(Duration::from_millis(20), waiting)
            .await
            .is_err());

        drop(first);
        let second = acquire_admission_permit(&state)
            .await
            .expect("second permit should acquire after release");
        drop(second);
        assert_eq!(state.available_admission_permits(), 1);
    }
}
