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
use tokio::sync::Mutex;

use crate::error::{ApiError, ApiResult};

const DEFAULT_SYSTEM_PROMPT: &str = "You are a helpful assistant.";
const DEFAULT_USER_PROMPT: &str = "What is the text in the illustrate?";
const END_TOKEN_ENV: &str = "MINERU_VLM_END_TOKEN";

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
    pub server_url: Option<String>,
    pub prompt: String,
    pub image_png: Option<Vec<u8>>,
    pub sampling_params: SamplingParams,
    pub priority: Option<i32>,
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

    /// Send an OpenAI-compatible chat completion request for one MinerU VLM step.
    ///
    /// Inputs:
    /// - `request`: prompt, optional image, sampling params, and server URL.
    pub async fn predict(&self, request: VlmRequest) -> ApiResult<String> {
        let started_at = Instant::now();
        let base_url = resolve_server_url(request.server_url.as_deref())?;
        let prompt_kind = prompt_kind(&request.prompt);
        let image_bytes = request.image_png.as_ref().map(Vec::len).unwrap_or_default();
        let model_name = self.resolve_model_name_cached(&base_url).await?;
        let body = build_chat_body(&model_name, &request);
        let request_started_at = Instant::now();
        let data: Value = self
            .openai_client(&base_url)
            .chat()
            .create_byot(body)
            .await?;
        let request_elapsed_ms = request_started_at.elapsed().as_millis();
        let content = parse_chat_content(&data)?;
        tracing::debug!(
            base_url = %base_url,
            model_name = %model_name,
            prompt_kind,
            image_bytes,
            content_chars = content.chars().count(),
            request_ms = request_elapsed_ms,
            elapsed_ms = started_at.elapsed().as_millis(),
            "vlm chat completion succeeded"
        );
        Ok(content)
    }

    /// Resolve and cache the model name for one OpenAI-compatible VLM server.
    ///
    /// Inputs:
    /// - `base_url`: normalized server origin without `/v1`.
    async fn resolve_model_name_cached(&self, base_url: &str) -> ApiResult<String> {
        let mut cache = self.model_name_cache.lock().await;
        if let Some(model_name) = cache.get(base_url).cloned() {
            tracing::debug!(
                base_url,
                model_name = %model_name,
                "vlm model name resolved from cache"
            );
            return Ok(model_name);
        }

        let started_at = Instant::now();
        let model_name = self.resolve_model_name(base_url).await?;
        tracing::debug!(
            base_url,
            model_name = %model_name,
            elapsed_ms = started_at.elapsed().as_millis(),
            "vlm model name resolved"
        );
        Ok(cache
            .entry(base_url.to_string())
            .or_insert(model_name)
            .clone())
    }

    async fn resolve_model_name(&self, base_url: &str) -> ApiResult<String> {
        if let Ok(model_name) = env::var("MINERU_VL_MODEL_NAME") {
            let trimmed = model_name.trim();
            if !trimmed.is_empty() {
                self.check_model_name(base_url, trimmed).await?;
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

    async fn check_model_name(&self, base_url: &str, model_name: &str) -> ApiResult<()> {
        let payload = self.list_models(base_url).await?;
        let models = model_list_from_payload(base_url, &payload)?;
        if models.iter().any(|id| id == model_name) {
            return Ok(());
        }
        Err(ApiError::BadRequest(format!(
            "Model '{model_name}' not found in the response from {base_url}/v1/models. Please check if the model is available on the server."
        )))
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

fn build_chat_body(model_name: &str, request: &VlmRequest) -> Value {
    let model_is_gpt = model_name.to_ascii_lowercase().starts_with("gpt");
    let mut user_content = Vec::new();
    if let Some(image_png) = &request.image_png {
        user_content.push(json!({
            "type": "image_url",
            "image_url": { "url": format!("data:image/png;base64,{}", STANDARD.encode(image_png)) }
        }));
    }
    user_content.push(json!({
        "type": "text",
        "text": if request.prompt.is_empty() { DEFAULT_USER_PROMPT } else { &request.prompt }
    }));

    let mut body = json!({
        "model": model_name,
        "messages": [
            { "role": "system", "content": DEFAULT_SYSTEM_PROMPT },
            { "role": "user", "content": user_content }
        ]
    });
    let object = body.as_object_mut().expect("body is an object");
    insert_sampling_params(object, &request.sampling_params, model_is_gpt);
    if let Some(priority) = request.priority {
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
    use std::{
        env,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
    };

    use axum::{routing::get, Json, Router};
    use futures::future::join_all;
    use serde_json::json;
    use tokio::{net::TcpListener, sync::oneshot, time::Duration};

    use super::{parse_chat_content, VlmHttpClient};

    static TEST_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    struct EnvVarGuard {
        name: &'static str,
        previous: Option<String>,
    }

    impl EnvVarGuard {
        fn set(name: &'static str, value: &str) -> Self {
            let previous = env::var(name).ok();
            env::set_var(name, value);
            Self { name, previous }
        }

        fn unset(name: &'static str) -> Self {
            let previous = env::var(name).ok();
            env::remove_var(name);
            Self { name, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(value) = &self.previous {
                env::set_var(self.name, value);
            } else {
                env::remove_var(self.name);
            }
        }
    }

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

    #[tokio::test]
    async fn caches_resolved_model_name_across_concurrent_requests() {
        let _env_lock = TEST_ENV_LOCK.lock().await;
        let _model_name = EnvVarGuard::unset("MINERU_VL_MODEL_NAME");
        let _no_proxy = EnvVarGuard::set("NO_PROXY", "127.0.0.1,localhost");
        let counter = Arc::new(AtomicUsize::new(0));
        let (base_url, server) = spawn_models_server(counter.clone()).await;
        let client = VlmHttpClient::new();

        let results = join_all((0..16).map(|_| client.resolve_model_name_cached(&base_url))).await;

        server.abort();
        for result in results {
            assert_eq!(result.unwrap(), "cached-model");
        }
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }
}
