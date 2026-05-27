use std::{path::PathBuf, sync::Arc};

use tokio::{fs, sync::Semaphore, time};

use crate::{
    config::{CliArgs, ServiceConfig},
    domain::tasks::TaskManager,
    error::{ApiError, ApiResult},
    vlm::{client::VlmHttpClient, parser::VlmDocumentParser},
};

#[derive(Clone)]
pub struct AppState {
    inner: Arc<AppStateInner>,
}

pub struct AppStateInner {
    pub config: ServiceConfig,
    pub task_manager: Arc<TaskManager>,
    pub parser: Arc<VlmDocumentParser>,
    pub admission_semaphore: Arc<Semaphore>,
    pub request_semaphore: Arc<Semaphore>,
}

impl AppState {
    /// Initialize all shared service objects and background maintenance tasks.
    ///
    /// Inputs:
    /// - `args`: command-line service settings.
    pub async fn new(args: CliArgs) -> ApiResult<Self> {
        let config = ServiceConfig::from_args(&args);
        fs::create_dir_all(&config.output_root)
            .await
            .map_err(ApiError::from)?;
        let task_manager = Arc::new(TaskManager::new(config.task_retention));
        let client = Arc::new(VlmHttpClient::new());
        let parser = Arc::new(VlmDocumentParser::new(
            client,
            config.processing_window_size,
            config.vlm_max_concurrency,
            config.image_preprocess_threads,
        )?);
        let admission_semaphore = Arc::new(Semaphore::new(config.max_in_flight_tasks));
        let request_semaphore = Arc::new(Semaphore::new(config.max_concurrent_requests));
        let state = Self {
            inner: Arc::new(AppStateInner {
                config,
                task_manager,
                parser,
                admission_semaphore,
                request_semaphore,
            }),
        };
        state.spawn_cleanup_loop();
        Ok(state)
    }

    pub fn config(&self) -> &ServiceConfig {
        &self.inner.config
    }

    pub fn task_manager(&self) -> Arc<TaskManager> {
        self.inner.task_manager.clone()
    }

    pub fn parser(&self) -> Arc<VlmDocumentParser> {
        self.inner.parser.clone()
    }

    pub fn admission_semaphore(&self) -> Arc<Semaphore> {
        self.inner.admission_semaphore.clone()
    }

    pub fn available_admission_permits(&self) -> usize {
        self.inner.admission_semaphore.available_permits()
    }

    pub fn available_vlm_permits(&self) -> usize {
        self.inner.parser.available_vlm_permits()
    }

    pub fn request_semaphore(&self) -> Arc<Semaphore> {
        self.inner.request_semaphore.clone()
    }

    pub fn create_task_output_dir(&self, task_id: uuid::Uuid) -> PathBuf {
        self.inner.config.output_root.join(task_id.to_string())
    }

    fn spawn_cleanup_loop(&self) {
        let task_manager = self.task_manager();
        let interval_duration = self.inner.config.task_cleanup_interval;
        if self.inner.config.task_retention.is_zero() {
            return;
        }
        tokio::spawn(async move {
            let mut interval = time::interval(interval_duration);
            loop {
                interval.tick().await;
                for path in task_manager.cleanup_expired().await {
                    if let Err(error) = fs::remove_dir_all(&path).await {
                        tracing::warn!(path = %path.display(), %error, "failed to clean expired task directory");
                    }
                }
            }
        });
    }
}
