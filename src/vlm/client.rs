use std::{env, sync::OnceLock, time::Duration};

use base64::{engine::general_purpose::STANDARD, Engine};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::{ApiError, ApiResult};

const DEFAULT_SYSTEM_PROMPT: &str = "You are a helpful assistant.";
const DEFAULT_USER_PROMPT: &str = "What is the text in the illustrate?";
const END_TOKEN_ENV: &str = "MINERU_VLM_END_TOKEN";

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

#[derive(Clone)]
pub struct VlmHttpClient {
    client: reqwest::Client,
}

impl VlmHttpClient {
    pub fn new() -> Self {
        let mut headers = HeaderMap::new();
        if let Ok(api_key) = env::var("MINERU_VL_API_KEY") {
            let trimmed = api_key.trim();
            if !trimmed.is_empty() {
                let value = format!("Bearer {trimmed}");
                if let Ok(header_value) = HeaderValue::from_str(&value) {
                    headers.insert(AUTHORIZATION, header_value);
                }
            }
        }
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(read_u64_env(
                "MINERU_HTTP_TIMEOUT",
                600,
            )))
            .build()
            .expect("reqwest client configuration must be valid");
        Self { client }
    }

    /// Send an OpenAI-compatible chat completion request for one MinerU VLM step.
    ///
    /// Inputs:
    /// - `request`: prompt, optional image, sampling params, and server URL.
    pub async fn predict(&self, request: VlmRequest) -> ApiResult<String> {
        let base_url = resolve_server_url(request.server_url.as_deref())?;
        let model_name = self.resolve_model_name(&base_url).await?;
        let body = build_chat_body(&model_name, &request);
        let response = self
            .client
            .post(format!("{base_url}/v1/chat/completions"))
            .json(&body)
            .send()
            .await?;
        let status = response.status();
        let text = response.text().await?;
        if !status.is_success() {
            return Err(ApiError::Internal(format!(
                "Unexpected status code: [{}], response body: {}",
                status.as_u16(),
                text
            )));
        }
        let data: Value = serde_json::from_str(&text).map_err(|error| {
            ApiError::Internal(format!(
                "Failed to parse response JSON: {error}, response body: {text}"
            ))
        })?;
        parse_chat_content(&data)
    }

    async fn resolve_model_name(&self, base_url: &str) -> ApiResult<String> {
        if let Ok(model_name) = env::var("MINERU_VL_MODEL_NAME") {
            let trimmed = model_name.trim();
            if !trimmed.is_empty() {
                self.check_model_name(base_url, trimmed).await?;
                return Ok(trimmed.to_string());
            }
        }
        let response = self
            .client
            .get(format!("{base_url}/v1/models"))
            .send()
            .await?;
        let status = response.status();
        let text = response.text().await?;
        if !status.is_success() {
            return Err(ApiError::Internal(format!(
                "Failed to get model name from {base_url}. Status code: {}, response body: {text}",
                status.as_u16()
            )));
        }
        let payload: ModelsResponse = serde_json::from_str(&text).map_err(|_| {
            ApiError::Internal(format!(
                "No models found in response from {base_url}. Response body: {text}"
            ))
        })?;
        if payload.data.len() != 1 {
            return Err(ApiError::BadRequest(format!(
                "Expected exactly one model from {base_url}, but got {}. Please specify the model name or set the `MINERU_VL_MODEL_NAME` environment variable.",
                payload.data.len()
            )));
        }
        payload
            .data
            .first()
            .map(|model| model.id.clone())
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                ApiError::BadRequest(format!(
                    "Model name is empty in response from {base_url}. Response body: {text}"
                ))
            })
    }

    async fn check_model_name(&self, base_url: &str, model_name: &str) -> ApiResult<()> {
        let response = self
            .client
            .get(format!("{base_url}/v1/models"))
            .send()
            .await?;
        let status = response.status();
        let text = response.text().await?;
        if !status.is_success() {
            return Err(ApiError::Internal(format!(
                "Failed to get model name from {base_url}. Status code: {}, response body: {text}",
                status.as_u16()
            )));
        }
        let payload: ModelsResponse = serde_json::from_str(&text).map_err(|_| {
            ApiError::Internal(format!(
                "No models found in response from {base_url}. Response body: {text}"
            ))
        })?;
        if payload.data.iter().any(|model| model.id == model_name) {
            return Ok(());
        }
        Err(ApiError::BadRequest(format!(
            "Model '{model_name}' not found in the response from {base_url}/v1/models. Please check if the model is available on the server."
        )))
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
        ],
        "skip_special_tokens": false
    });
    let object = body.as_object_mut().expect("body is an object");
    insert_sampling_params(object, &request.sampling_params);
    if let Some(priority) = request.priority {
        object.insert("priority".to_string(), json!(priority));
    }
    body
}

fn insert_sampling_params(object: &mut serde_json::Map<String, Value>, params: &SamplingParams) {
    if let Some(value) = params.temperature {
        object.insert("temperature".to_string(), json!(value));
    }
    if let Some(value) = params.top_p {
        object.insert("top_p".to_string(), json!(value));
    }
    if let Some(value) = params.top_k {
        object.insert("top_k".to_string(), json!(value));
    }
    if let Some(value) = params.presence_penalty {
        object.insert("presence_penalty".to_string(), json!(value));
    }
    if let Some(value) = params.frequency_penalty {
        object.insert("frequency_penalty".to_string(), json!(value));
    }
    if let Some(value) = params.repetition_penalty {
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

#[derive(Debug, Deserialize)]
struct ModelsResponse {
    data: Vec<ModelInfo>,
}

#[derive(Debug, Deserialize)]
struct ModelInfo {
    id: String,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::parse_chat_content;

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
}
