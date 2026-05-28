use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Instant,
};

use tokio::sync::{mpsc, oneshot, Mutex};
use uuid::Uuid;

use crate::error::{ApiError, ApiResult};

use super::client::{VlmHttpClient, VlmRequest, VlmSession};

#[derive(Debug, Clone, Copy)]
pub struct VlmSchedulerStats {
    pub queue_depth: usize,
    pub active_requests: usize,
    pub queue_capacity: usize,
}

struct ScheduledVlmRequest {
    task_id: Uuid,
    session: VlmSession,
    request: VlmRequest,
    enqueued_at: Instant,
    response: oneshot::Sender<ApiResult<String>>,
}

pub struct VlmRequestScheduler {
    sender: mpsc::Sender<ScheduledVlmRequest>,
    queue_depth: Arc<AtomicUsize>,
    active_requests: Arc<AtomicUsize>,
    queue_capacity: usize,
    max_concurrency: usize,
}

impl VlmRequestScheduler {
    /// Build a bounded global VLM request scheduler and start worker tasks.
    ///
    /// Inputs:
    /// - `client`: shared VLM HTTP client used by all workers.
    /// - `max_concurrency`: number of worker tasks sending requests to VLM.
    /// - `queue_capacity`: maximum queued VLM jobs waiting for workers.
    pub fn new(
        client: Arc<VlmHttpClient>,
        max_concurrency: usize,
        queue_capacity: usize,
    ) -> Arc<Self> {
        let max_concurrency = max_concurrency.max(1);
        let queue_capacity = queue_capacity.max(1);
        let (sender, receiver) = mpsc::channel(queue_capacity);
        let queue_depth = Arc::new(AtomicUsize::new(0));
        let active_requests = Arc::new(AtomicUsize::new(0));
        let shared_receiver = Arc::new(Mutex::new(receiver));
        for worker_index in 0..max_concurrency {
            let receiver = shared_receiver.clone();
            let client = client.clone();
            let queue_depth = queue_depth.clone();
            let active_requests = active_requests.clone();
            tokio::spawn(async move {
                run_vlm_worker(worker_index, receiver, client, queue_depth, active_requests).await;
            });
        }
        Arc::new(Self {
            sender,
            queue_depth,
            active_requests,
            queue_capacity,
            max_concurrency,
        })
    }

    /// Submit one VLM request to the bounded global worker queue.
    ///
    /// Inputs:
    /// - `task_id`: parse task identifier used for fairness and logging.
    /// - `session`: resolved VLM backend/model context.
    /// - `request`: prompt, image bytes, sampling options, and backend priority.
    pub async fn predict(
        &self,
        task_id: Uuid,
        session: &VlmSession,
        request: VlmRequest,
    ) -> ApiResult<String> {
        let submit_started_at = Instant::now();
        let image_bytes = request.image_png.as_ref().map(Vec::len).unwrap_or_default();
        let (response, receiver) = oneshot::channel();
        let permit = self.sender.reserve().await.map_err(|_| {
            ApiError::ServiceUnavailable("VLM request scheduler stopped".to_string())
        })?;
        let queue_capacity_wait_ms = submit_started_at.elapsed().as_millis();
        self.queue_depth.fetch_add(1, Ordering::SeqCst);
        permit.send(ScheduledVlmRequest {
            task_id,
            session: session.clone(),
            request,
            enqueued_at: Instant::now(),
            response,
        });
        tracing::debug!(
            %task_id,
            image_bytes,
            queue_depth = self.queue_depth(),
            queue_capacity_wait_ms,
            "vlm request enqueued"
        );
        receiver
            .await
            .map_err(|_| ApiError::ServiceUnavailable("VLM request worker stopped".to_string()))?
    }

    pub fn stats(&self) -> VlmSchedulerStats {
        VlmSchedulerStats {
            queue_depth: self.queue_depth(),
            active_requests: self.active_requests(),
            queue_capacity: self.queue_capacity,
        }
    }

    pub fn available_permits(&self) -> usize {
        self.max_concurrency
            .saturating_sub(self.active_requests.load(Ordering::SeqCst))
    }

    pub fn queue_depth(&self) -> usize {
        self.queue_depth.load(Ordering::SeqCst)
    }

    pub fn active_requests(&self) -> usize {
        self.active_requests.load(Ordering::SeqCst)
    }
}

async fn run_vlm_worker(
    worker_index: usize,
    receiver: Arc<Mutex<mpsc::Receiver<ScheduledVlmRequest>>>,
    client: Arc<VlmHttpClient>,
    queue_depth: Arc<AtomicUsize>,
    active_requests: Arc<AtomicUsize>,
) {
    loop {
        let Some(job) = receive_next_job(&receiver).await else {
            break;
        };
        queue_depth.fetch_sub(1, Ordering::SeqCst);
        let queue_wait_ms = job.enqueued_at.elapsed().as_millis();
        active_requests.fetch_add(1, Ordering::SeqCst);
        let request_started_at = Instant::now();
        let result = client.predict_with_session(&job.session, job.request).await;
        active_requests.fetch_sub(1, Ordering::SeqCst);
        tracing::debug!(
            task_id = %job.task_id,
            worker_index,
            queue_wait_ms,
            worker_request_ms = request_started_at.elapsed().as_millis(),
            ok = result.is_ok(),
            "vlm worker request completed"
        );
        let _ = job.response.send(result);
    }
}

async fn receive_next_job(
    receiver: &Arc<Mutex<mpsc::Receiver<ScheduledVlmRequest>>>,
) -> Option<ScheduledVlmRequest> {
    let mut receiver = receiver.lock().await;
    receiver.recv().await
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        time::Duration,
    };

    use axum::{extract::State, http::StatusCode, routing::get, routing::post, Json, Router};
    use serde_json::{json, Value};
    use tokio::{net::TcpListener, sync::oneshot, time::sleep};

    use crate::vlm::{
        client::{sampling_params_for_block, VlmHttpClient, VlmRequest},
        test_env::{EnvVarGuard, TEST_ENV_LOCK},
    };

    use super::VlmRequestScheduler;

    #[derive(Clone)]
    struct SchedulerTestState {
        active: Arc<AtomicUsize>,
        max_active: Arc<AtomicUsize>,
    }

    #[tokio::test]
    async fn scheduler_limits_active_requests_to_worker_count() {
        let _env_lock = TEST_ENV_LOCK.lock().await;
        let _model_name = EnvVarGuard::set("MINERU_VL_MODEL_NAME", "test-model");
        let _no_proxy = EnvVarGuard::set("NO_PROXY", "127.0.0.1,localhost");
        let (base_url, state, server) = spawn_scheduler_test_server().await;
        let client = Arc::new(VlmHttpClient::new());
        let session = client
            .session_for_request(Some(&base_url))
            .await
            .expect("session should resolve");
        let scheduler = VlmRequestScheduler::new(client, 2, 8);

        let mut requests = Vec::new();
        for _ in 0..6 {
            requests.push(scheduler.predict(
                uuid::Uuid::new_v4(),
                &session,
                VlmRequest {
                    prompt: "\nText Recognition:".to_string(),
                    image_png: None,
                    sampling_params: sampling_params_for_block("text"),
                    priority: None,
                },
            ));
        }
        let results = futures::future::join_all(requests).await;

        server.abort();
        assert!(results.into_iter().all(|result| result.is_ok()));
        assert_eq!(state.max_active.load(Ordering::SeqCst), 2);
        assert_eq!(scheduler.stats().queue_capacity, 8);
    }

    #[tokio::test]
    async fn scheduler_reaches_global_limit_when_queue_has_work() {
        let _env_lock = TEST_ENV_LOCK.lock().await;
        let _model_name = EnvVarGuard::set("MINERU_VL_MODEL_NAME", "test-model");
        let _no_proxy = EnvVarGuard::set("NO_PROXY", "127.0.0.1,localhost");
        let (base_url, state, server) = spawn_scheduler_test_server().await;
        let client = Arc::new(VlmHttpClient::new());
        let session = client
            .session_for_request(Some(&base_url))
            .await
            .expect("session should resolve");
        let scheduler = VlmRequestScheduler::new(client, 3, 8);

        let mut requests = Vec::new();
        for _ in 0..8 {
            requests.push(scheduler.predict(
                uuid::Uuid::new_v4(),
                &session,
                VlmRequest {
                    prompt: "\nText Recognition:".to_string(),
                    image_png: None,
                    sampling_params: sampling_params_for_block("text"),
                    priority: None,
                },
            ));
        }
        let results = futures::future::join_all(requests).await;

        server.abort();
        assert!(results.into_iter().all(|result| result.is_ok()));
        assert_eq!(state.max_active.load(Ordering::SeqCst), 3);
    }

    async fn spawn_scheduler_test_server(
    ) -> (String, SchedulerTestState, tokio::task::JoinHandle<()>) {
        let state = SchedulerTestState {
            active: Arc::new(AtomicUsize::new(0)),
            max_active: Arc::new(AtomicUsize::new(0)),
        };
        let app = Router::new()
            .route("/v1/models", post(test_models).get(test_models))
            .route("/v1/chat/completions", post(test_chat_completions))
            .route("/ready", get(|| async { "ok" }))
            .with_state(state.clone());
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
        ready_receiver.await.expect("server should start");
        wait_until_ready(&base_url).await;
        (base_url, state, server)
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
            sleep(Duration::from_millis(10)).await;
        }
        panic!("test server did not become ready");
    }

    async fn test_models() -> Json<Value> {
        Json(json!({
            "object": "list",
            "data": [{ "id": "test-model", "object": "model" }]
        }))
    }

    async fn test_chat_completions(
        State(state): State<SchedulerTestState>,
        Json(_payload): Json<Value>,
    ) -> (StatusCode, Json<Value>) {
        let active = state.active.fetch_add(1, Ordering::SeqCst) + 1;
        state.max_active.fetch_max(active, Ordering::SeqCst);
        sleep(Duration::from_millis(60)).await;
        state.active.fetch_sub(1, Ordering::SeqCst);
        (
            StatusCode::OK,
            Json(json!({
                "id": "chatcmpl-test",
                "object": "chat.completion",
                "choices": [{
                    "index": 0,
                    "finish_reason": "stop",
                    "message": { "role": "assistant", "content": "recognized text" }
                }]
            })),
        )
    }
}
