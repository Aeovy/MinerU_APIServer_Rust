use std::{
    collections::HashMap,
    env,
    sync::OnceLock,
    time::{Duration, Instant},
};

use async_openai::{config::Config, Client};
use base64::{engine::general_purpose::STANDARD, Engine};
use reqwest::header::{HeaderMap, AUTHORIZATION};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{json, Value};
use tokio::{sync::Mutex, time::sleep};

use crate::error::{ApiError, ApiResult};

const DEFAULT_SYSTEM_PROMPT: &str = "You are a helpful assistant.";
const DEFAULT_USER_PROMPT: &str = "What is the text in the illustrate?";
const END_TOKEN_ENV: &str = "MINERU_VLM_END_TOKEN";
const CHAT_COMPLETION_MAX_ATTEMPTS: usize = 3;
const CHAT_COMPLETION_RETRY_DELAYS_MS: [u64; 2] = [200, 500];

#[derive(Debug, Clone)]
struct MineruOpenAiConfig {
    api_base: String,
    api_key: SecretString,
}

impl MineruOpenAiConfig {
    fn new(base_url: &str, api_key: Option<&str>) -> Self {
        Self {
            api_base: format!("{}/v1", base_url.trim_end_matches('/')),
            api_key: api_key.unwrap_or_default().to_string().into(),
        }
    }
}

impl Config for MineruOpenAiConfig {
    fn headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let api_key = self.api_key.expose_secret();
        if !api_key.is_empty() {
            let value = format!("Bearer {api_key}");
            if let Ok(header_value) = value.parse() {
                headers.insert(AUTHORIZATION, header_value);
            }
        }
        headers
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.api_base, path)
    }

    fn query(&self) -> Vec<(&str, &str)> {
        Vec::new()
    }

    fn api_base(&self) -> &str {
        &self.api_base
    }

    fn api_key(&self) -> &SecretString {
        &self.api_key
    }
}

#[derive(Debug, Clone)]
pub struct SamplingParams {
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub top_k: Option<u32>,
    pub presence_penalty: Option<f32>,
    pub frequency_penalty: Option<f32>,
    pub repetition_penalty: Option<f32>,
    pub no_repeat_ngram_size: Option<u32>,
    pub max_new_tokens: Option<u32>,
}

impl SamplingParams {
    fn mineru_default(presence_penalty: f32, frequency_penalty: f32) -> Self {
        Self {
            temperature: Some(0.0),
            top_p: Some(0.01),
            top_k: Some(1),
            presence_penalty: Some(presence_penalty),
            frequency_penalty: Some(frequency_penalty),
            repetition_penalty: Some(1.0),
            no_repeat_ngram_size: Some(100),
            max_new_tokens: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct VlmRequest {
    pub prompt: String,
    pub image_png: Option<Vec<u8>>,
    pub sampling_params: SamplingParams,
    pub priority: Option<i32>,
}

#[derive(Debug, Clone)]
pub struct VlmSession {
    base_url: String,
    model_name: String,
}

pub struct VlmHttpClient {
    http_client: reqwest::Client,
    api_key: Option<String>,
    model_name_cache: Mutex<HashMap<String, String>>,
}

impl VlmHttpClient {
    pub fn new() -> Self {
        let api_key = read_optional_env("MINERU_VL_API_KEY");
        let http_client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(read_u64_env(
                "MINERU_HTTP_TIMEOUT",
                600,
            )))
            .build()
            .expect("reqwest client configuration must be valid");
        Self {
            http_client,
            api_key,
            model_name_cache: Mutex::new(HashMap::new()),
        }
    }

    /// Build a VLM session for one parser request so all chat calls reuse the same model.
    ///
    /// Inputs:
    /// - `server_url`: optional multipart server URL overriding the environment default.
    pub async fn session_for_request(&self, server_url: Option<&str>) -> ApiResult<VlmSession> {
        let base_url = resolve_server_url(server_url)?;
        self.session_for_base_url(&base_url).await
    }

    /// Send one chat completion through an already resolved VLM session.
    ///
    /// Inputs:
    /// - `session`: per-request server/model context.
    /// - `request`: prompt, optional image, sampling params, and priority.
    pub async fn predict_with_session(
        &self,
        session: &VlmSession,
        request: VlmRequest,
    ) -> ApiResult<String> {
        let started_at = Instant::now();
        let prompt_kind = prompt_kind(&request.prompt);
        let image_bytes = request.image_png.as_ref().map(Vec::len).unwrap_or_default();
        let body = build_chat_body(&session.model_name, request);
        let mut attempt_count = 0_usize;
        let (data, request_elapsed_ms) = loop {
            let attempt = attempt_count + 1;
            let request_started_at = Instant::now();
            match self
                .openai_client(&session.base_url)
                .chat()
                .create_byot(body.clone())
                .await
            {
                Ok(data) => {
                    let request_elapsed_ms = request_started_at.elapsed().as_millis();
                    if attempt > 1 {
                        tracing::debug!(
                            base_url = %session.base_url,
                            model_name = %session.model_name,
                            prompt_kind,
                            image_bytes,
                            attempt,
                            request_ms = request_elapsed_ms,
                            elapsed_ms = started_at.elapsed().as_millis(),
                            "vlm chat completion retry succeeded"
                        );
                    }
                    break (data, request_elapsed_ms);
                }
                Err(error)
                    if attempt < CHAT_COMPLETION_MAX_ATTEMPTS
                        && should_retry_chat_error(&error) =>
                {
                    let delay_ms = CHAT_COMPLETION_RETRY_DELAYS_MS
                        .get(attempt - 1)
                        .copied()
                        .unwrap_or(500);
                    tracing::debug!(
                        base_url = %session.base_url,
                        model_name = %session.model_name,
                        prompt_kind,
                        image_bytes,
                        attempt,
                        delay_ms,
                        request_ms = request_started_at.elapsed().as_millis(),
                        error = %error,
                        status = retry_error_status(&error).as_deref(),
                        code = retry_error_code(&error).as_deref(),
                        error_type = retry_error_type(&error).as_deref(),
                        "vlm chat completion retrying after retryable VLM error"
                    );
                    attempt_count += 1;
                    sleep(Duration::from_millis(delay_ms)).await;
                }
                Err(error) => {
                    tracing::debug!(
                        base_url = %session.base_url,
                        model_name = %session.model_name,
                        prompt_kind,
                        image_bytes,
                        attempt,
                        request_ms = request_started_at.elapsed().as_millis(),
                        elapsed_ms = started_at.elapsed().as_millis(),
                        error = %error,
                        "vlm chat completion failed"
                    );
                    return Err(ApiError::from(error));
                }
            }
        };
        let content = parse_chat_content(&data)?;
        tracing::debug!(
            base_url = %session.base_url,
            model_name = %session.model_name,
            prompt_kind,
            image_bytes,
            content_chars = content.chars().count(),
            request_ms = request_elapsed_ms,
            elapsed_ms = started_at.elapsed().as_millis(),
            "vlm chat completion succeeded"
        );
        Ok(content)
    }

    async fn session_for_base_url(&self, base_url: &str) -> ApiResult<VlmSession> {
        let model_name = self.resolve_model_name_cached(base_url).await?;
        Ok(VlmSession {
            base_url: base_url.to_string(),
            model_name,
        })
    }

    /// Resolve and cache the model name for one OpenAI-compatible VLM server.
    ///
    /// Inputs:
    /// - `base_url`: normalized server origin without `/v1`.
    async fn resolve_model_name_cached(&self, base_url: &str) -> ApiResult<String> {
        {
            let cache = self.model_name_cache.lock().await;
            if let Some(model_name) = cache.get(base_url).cloned() {
                tracing::trace!(
                    base_url,
                    model_name = %model_name,
                    "vlm model name resolved from cache"
                );
                return Ok(model_name);
            }
        }

        let started_at = Instant::now();
        let model_name = self.resolve_model_name(base_url).await?;
        tracing::debug!(
            base_url,
            model_name = %model_name,
            elapsed_ms = started_at.elapsed().as_millis(),
            "vlm model name resolved"
        );
        let mut cache = self.model_name_cache.lock().await;
        Ok(cache
            .entry(base_url.to_string())
            .or_insert(model_name)
            .clone())
    }

    async fn resolve_model_name(&self, base_url: &str) -> ApiResult<String> {
        if let Ok(model_name) = env::var("MINERU_VL_MODEL_NAME") {
            let trimmed = model_name.trim();
            if !trimmed.is_empty() {
                tracing::debug!(
                    base_url,
                    model_name = trimmed,
                    "vlm model name resolved from global environment override"
                );
                return Ok(trimmed.to_string());
            }
        }
        let payload = self.list_models(base_url).await?;
        let models = model_list_from_payload(base_url, &payload)?;
        if models.len() != 1 {
            return Err(ApiError::BadRequest(format!(
                "Expected exactly one model from {base_url}, but got {}. Please specify the model name or set the `MINERU_VL_MODEL_NAME` environment variable.",
                models.len()
            )));
        }
        models
            .first()
            .map(ToOwned::to_owned)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                ApiError::BadRequest(format!(
                    "Model name is empty in response from {base_url}. Response body: {payload}"
                ))
            })
    }

    async fn list_models(&self, base_url: &str) -> ApiResult<Value> {
        let started_at = Instant::now();
        let payload = self.openai_client(base_url).models().list_byot().await?;
        tracing::debug!(
            base_url,
            elapsed_ms = started_at.elapsed().as_millis(),
            "vlm models listed"
        );
        Ok(payload)
    }

    fn openai_client(&self, base_url: &str) -> Client<MineruOpenAiConfig> {
        Client::build(
            self.http_client.clone(),
            MineruOpenAiConfig::new(base_url, self.api_key.as_deref()),
        )
    }
}

pub fn layout_sampling_params() -> SamplingParams {
    SamplingParams::mineru_default(0.0, 0.0)
}

pub fn sampling_params_for_block(block_type: &str) -> SamplingParams {
    match block_type {
        "table" => SamplingParams::mineru_default(1.0, 0.005),
        "equation" | "image" | "chart" => SamplingParams::mineru_default(1.0, 0.05),
        _ => SamplingParams::mineru_default(1.0, 0.05),
    }
}

pub fn prompt_for_block(block_type: &str) -> &'static str {
    match block_type {
        "table" => "\nTable Recognition:",
        "equation" => "\nFormula Recognition:",
        "image" | "chart" => "\nImage Analysis:",
        _ => "\nText Recognition:",
    }
}

pub fn layout_prompt() -> &'static str {
    "\nLayout Detection:"
}

fn prompt_kind(prompt: &str) -> &'static str {
    if prompt.contains("Layout Detection") {
        "layout"
    } else if prompt.contains("Table Recognition") {
        "table"
    } else if prompt.contains("Formula Recognition") {
        "formula"
    } else if prompt.contains("Image Analysis") {
        "image"
    } else {
        "text"
    }
}

fn should_retry_chat_error(error: &async_openai::error::OpenAIError) -> bool {
    match error {
        async_openai::error::OpenAIError::Reqwest(error) => {
            error.is_request() || error.is_connect() || error.is_timeout()
        }
        async_openai::error::OpenAIError::ApiError(error) => {
            error.status_code == reqwest::StatusCode::TOO_MANY_REQUESTS
                || error.status_code.is_server_error()
                || error.api_error.code.as_deref() == Some("rate_limit_exceeded")
                || error.api_error.r#type.as_deref() == Some("server_error")
        }
        _ => false,
    }
}

fn retry_error_status(error: &async_openai::error::OpenAIError) -> Option<String> {
    match error {
        async_openai::error::OpenAIError::Reqwest(error) => {
            error.status().map(|status| status.to_string())
        }
        async_openai::error::OpenAIError::ApiError(error) => Some(error.status_code.to_string()),
        _ => None,
    }
}

fn retry_error_code(error: &async_openai::error::OpenAIError) -> Option<String> {
    match error {
        async_openai::error::OpenAIError::ApiError(error) => error.api_error.code.clone(),
        _ => None,
    }
}

fn retry_error_type(error: &async_openai::error::OpenAIError) -> Option<String> {
    match error {
        async_openai::error::OpenAIError::ApiError(error) => error.api_error.r#type.clone(),
        _ => None,
    }
}

pub fn parse_chat_content(data: &Value) -> ApiResult<String> {
    if data.get("object").and_then(Value::as_str) == Some("error") {
        return Err(ApiError::Internal(format!("Error from server: {data}")));
    }
    let choice = data
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(|| ApiError::Internal("No choices found in the response.".to_string()))?;
    let finish_reason = choice.get("finish_reason").and_then(Value::as_str);
    match finish_reason {
        Some("stop") | Some("length") => {}
        Some(other) => {
            return Err(ApiError::BadRequest(format!(
                "Unexpected finish reason: {other}"
            )));
        }
        None => {
            return Err(ApiError::Internal(
                "Finish reason is None in the response.".to_string(),
            ));
        }
    }
    let content = choice
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let end_token = env::var(END_TOKEN_ENV).unwrap_or_else(|_| "<|im_end|>".to_string());
    Ok(content
        .strip_suffix(&end_token)
        .unwrap_or(content)
        .to_string())
}

fn build_chat_body(model_name: &str, request: VlmRequest) -> Value {
    let VlmRequest {
        prompt,
        image_png,
        sampling_params,
        priority,
    } = request;
    let model_is_gpt = model_name.to_ascii_lowercase().starts_with("gpt");
    let mut user_content = Vec::new();
    if let Some(image_png) = image_png {
        user_content.push(json!({
            "type": "image_url",
            "image_url": { "url": format!("data:image/png;base64,{}", STANDARD.encode(image_png)) }
        }));
    }
    user_content.push(json!({
        "type": "text",
        "text": if prompt.is_empty() { DEFAULT_USER_PROMPT } else { &prompt }
    }));

    let mut body = json!({
        "model": model_name,
        "messages": [
            { "role": "system", "content": DEFAULT_SYSTEM_PROMPT },
            { "role": "user", "content": user_content }
        ]
    });
    let object = body.as_object_mut().expect("body is an object");
    insert_sampling_params(object, &sampling_params, model_is_gpt);
    if let Some(priority) = priority {
        object.insert("priority".to_string(), json!(priority));
    }
    body
}

fn insert_sampling_params(
    object: &mut serde_json::Map<String, Value>,
    params: &SamplingParams,
    model_is_gpt: bool,
) {
    if let Some(value) = params.temperature {
        object.insert("temperature".to_string(), json!(value));
    }
    if let Some(value) = params.top_p {
        object.insert("top_p".to_string(), json!(value));
    }
    if !model_is_gpt {
        object.insert("skip_special_tokens".to_string(), json!(false));
    }
    if let (false, Some(value)) = (model_is_gpt, params.top_k) {
        object.insert("top_k".to_string(), json!(value));
    }
    if let Some(value) = params.presence_penalty {
        object.insert("presence_penalty".to_string(), json!(value));
    }
    if let Some(value) = params.frequency_penalty {
        object.insert("frequency_penalty".to_string(), json!(value));
    }
    if let (false, Some(value)) = (model_is_gpt, params.repetition_penalty) {
        object.insert("repetition_penalty".to_string(), json!(value));
    }
    if let Some(value) = params.no_repeat_ngram_size {
        object.insert(
            "vllm_xargs".to_string(),
            json!({ "no_repeat_ngram_size": value, "debug": false }),
        );
    }
    if let Some(value) = params.max_new_tokens {
        object.insert("max_completion_tokens".to_string(), json!(value));
        object.insert("max_tokens".to_string(), json!(value));
    }
}

fn resolve_server_url(server_url: Option<&str>) -> ApiResult<String> {
    let raw = server_url
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| env::var("MINERU_VL_SERVER").ok())
        .ok_or_else(|| {
            ApiError::BadRequest("Environment variable MINERU_VL_SERVER is not set.".to_string())
        })?;
    let trimmed = raw.trim().trim_end_matches('/').to_string();
    let base = if trimmed.ends_with("/v1") {
        trimmed.trim_end_matches("/v1").to_string()
    } else {
        extract_origin(&trimmed)?
    };
    Ok(base)
}

fn extract_origin(server_url: &str) -> ApiResult<String> {
    static ORIGIN_RE: OnceLock<regex::Regex> = OnceLock::new();
    let regex = ORIGIN_RE.get_or_init(|| {
        regex::Regex::new(r"^(https?://[^/]+)").expect("origin regex must compile")
    });
    regex
        .captures(server_url)
        .and_then(|captures| captures.get(1))
        .map(|match_| match_.as_str().to_string())
        .ok_or_else(|| ApiError::BadRequest(format!("Invalid server URL: {server_url}")))
}

fn read_u64_env(name: &str, default: u64) -> u64 {
    env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(default)
}

fn read_optional_env(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn model_list_from_payload(base_url: &str, payload: &Value) -> ApiResult<Vec<String>> {
    let models = payload
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            ApiError::Internal(format!(
                "No models found in response from {base_url}. Response body: {payload}"
            ))
        })?;
    Ok(models
        .iter()
        .filter_map(|model| {
            model
                .get("id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    use axum::{extract::State, http::StatusCode, routing::get, routing::post, Json, Router};
    use serde_json::json;
    use tokio::{net::TcpListener, sync::oneshot, time::Duration};

    use crate::vlm::test_env::{EnvVarGuard, TEST_ENV_LOCK};

    use super::{
        build_chat_body, parse_chat_content, sampling_params_for_block, VlmHttpClient, VlmRequest,
        VlmSession,
    };

    async fn spawn_models_server(
        counter: Arc<AtomicUsize>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let app = Router::new()
            .route(
                "/v1/models",
                get({
                    let counter = counter.clone();
                    move || {
                        let counter = counter.clone();
                        async move {
                            counter.fetch_add(1, Ordering::SeqCst);
                            Json(json!({
                                "object": "list",
                                "data": [{ "id": "cached-model", "object": "model" }]
                            }))
                        }
                    }
                }),
            )
            .route("/ready", get(|| async { "ok" }));
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test server must bind");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        let (ready_sender, ready_receiver) = oneshot::channel();
        let server = tokio::spawn(async move {
            let _ = ready_sender.send(());
            axum::serve(listener, app)
                .await
                .expect("test server must run");
        });
        ready_receiver.await.expect("test server must start");
        wait_until_ready(&base_url).await;
        (base_url, server)
    }

    async fn spawn_retry_chat_server(
        failures_before_success: usize,
        status: StatusCode,
    ) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let counter = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route("/v1/chat/completions", post(test_retry_chat_completion))
            .route("/ready", get(|| async { "ok" }))
            .with_state(RetryChatState {
                counter: counter.clone(),
                failures_before_success,
                status,
            });
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test server must bind");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        let (ready_sender, ready_receiver) = oneshot::channel();
        let server = tokio::spawn(async move {
            let _ = ready_sender.send(());
            axum::serve(listener, app)
                .await
                .expect("test server must run");
        });
        ready_receiver.await.expect("test server must start");
        wait_until_ready(&base_url).await;
        (base_url, counter, server)
    }

    async fn wait_until_ready(base_url: &str) {
        let ready_url = format!("{base_url}/ready");
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("ready client should build");
        for _ in 0..100 {
            if let Ok(response) = client.get(&ready_url).send().await {
                if response.status().is_success() {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("test server did not become ready");
    }

    #[test]
    fn parses_stop_content() {
        let payload = json!({
            "choices": [{"finish_reason": "stop", "message": {"content": "ok<|im_end|>"}}]
        });
        assert_eq!(parse_chat_content(&payload).unwrap(), "ok");
    }

    #[test]
    fn rejects_empty_choices() {
        let payload = json!({ "choices": [] });
        assert!(parse_chat_content(&payload).is_err());
    }

    #[test]
    fn chat_body_consumes_image_bytes_into_base64_url() {
        let body = build_chat_body(
            "test-model",
            VlmRequest {
                prompt: "\nText Recognition:".to_string(),
                image_png: Some(vec![1_u8, 2, 3]),
                sampling_params: sampling_params_for_block("text"),
                priority: Some(7),
            },
        );

        assert_eq!(body["priority"], 7);
        assert_eq!(
            body["messages"][1]["content"][0]["image_url"]["url"],
            "data:image/png;base64,AQID"
        );
        assert_eq!(
            body["messages"][1]["content"][1]["text"],
            "\nText Recognition:"
        );
    }

    #[tokio::test]
    async fn caches_resolved_model_name_after_first_resolution() {
        let _env_lock = TEST_ENV_LOCK.lock().await;
        let _model_name = EnvVarGuard::unset("MINERU_VL_MODEL_NAME");
        let _no_proxy = EnvVarGuard::set("NO_PROXY", "127.0.0.1,localhost");
        let counter = Arc::new(AtomicUsize::new(0));
        let (base_url, server) = spawn_models_server(counter.clone()).await;
        let client = VlmHttpClient::new();

        let first = client
            .resolve_model_name_cached(&base_url)
            .await
            .expect("first model name should resolve");
        let second = client
            .resolve_model_name_cached(&base_url)
            .await
            .expect("second model name should resolve from cache");

        server.abort();
        assert_eq!(first, "cached-model");
        assert_eq!(second, "cached-model");
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn uses_configured_model_name_without_listing_models() {
        let _env_lock = TEST_ENV_LOCK.lock().await;
        let _model_name = EnvVarGuard::set("MINERU_VL_MODEL_NAME", "configured-model");
        let _no_proxy = EnvVarGuard::set("NO_PROXY", "127.0.0.1,localhost");
        let counter = Arc::new(AtomicUsize::new(0));
        let (base_url, server) = spawn_models_server(counter.clone()).await;
        let client = VlmHttpClient::new();

        let model_name = client
            .resolve_model_name_cached(&base_url)
            .await
            .expect("configured model name should resolve");

        server.abort();
        assert_eq!(model_name, "configured-model");
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn retries_retryable_api_status_before_success() {
        let (base_url, counter, server) =
            spawn_retry_chat_server(1, StatusCode::INTERNAL_SERVER_ERROR).await;
        let client = VlmHttpClient::new();
        let session = VlmSession {
            base_url,
            model_name: "test-model".to_string(),
        };

        let content = client
            .predict_with_session(
                &session,
                VlmRequest {
                    prompt: "\nText Recognition:".to_string(),
                    image_png: None,
                    sampling_params: sampling_params_for_block("text"),
                    priority: None,
                },
            )
            .await
            .expect("retry should eventually succeed");

        server.abort();
        assert_eq!(content, "recognized text");
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    #[derive(Clone)]
    struct RetryChatState {
        counter: Arc<AtomicUsize>,
        failures_before_success: usize,
        status: StatusCode,
    }

    async fn test_retry_chat_completion(
        State(state): State<RetryChatState>,
    ) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
        let attempt = state.counter.fetch_add(1, Ordering::SeqCst) + 1;
        if attempt <= state.failures_before_success {
            return Err((
                state.status,
                Json(json!({
                    "error": {
                        "message": "temporary VLM failure",
                        "type": "server_error",
                        "param": null,
                        "code": null
                    }
                })),
            ));
        }
        Ok(Json(json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "finish_reason": "stop",
                "message": { "role": "assistant", "content": "recognized text" }
            }]
        })))
    }
}
