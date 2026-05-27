use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use chrono::Utc;
use tokio::sync::{Notify, RwLock};
use uuid::Uuid;

use crate::domain::models::{ParseTask, StatusPayload, TaskStatus};

#[derive(Debug, Clone, Default)]
pub struct TaskStats {
    pub pending: usize,
    pub processing: usize,
    pub completed: usize,
    pub failed: usize,
}

#[derive(Debug)]
struct TaskEntry {
    task: ParseTask,
    notify: Arc<Notify>,
    active_result_readers: Arc<AtomicUsize>,
}

#[derive(Debug)]
pub struct ResultReadLease {
    active_result_readers: Arc<AtomicUsize>,
}

impl Drop for ResultReadLease {
    fn drop(&mut self) {
        self.active_result_readers.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use uuid::Uuid;

    use crate::domain::models::{ParseOptions, ParseTask};

    use super::TaskManager;

    #[tokio::test]
    async fn cleanup_skips_tasks_with_active_result_lease() {
        let manager = TaskManager::new(Duration::from_millis(1));
        let output_dir = tempfile::tempdir().expect("tempdir should be created");
        let task_id = Uuid::new_v4();
        let task = ParseTask::new(
            task_id,
            &ParseOptions::default(),
            Vec::new(),
            output_dir.path().to_path_buf(),
        );
        manager.submit(task).await;
        manager.set_completed(task_id, Vec::new()).await;
        tokio::time::sleep(Duration::from_millis(2)).await;

        let lease = manager
            .acquire_result_lease(task_id)
            .await
            .expect("lease should be acquired");
        assert!(manager.cleanup_expired().await.is_empty());

        drop(lease);
        assert_eq!(
            manager.cleanup_expired().await,
            vec![output_dir.path().to_path_buf()]
        );
    }
}

#[derive(Debug)]
pub struct TaskManager {
    tasks: RwLock<HashMap<Uuid, TaskEntry>>,
    next_submit_order: AtomicU64,
    retention: Duration,
}

impl TaskManager {
    pub fn new(retention: Duration) -> Self {
        Self {
            tasks: RwLock::new(HashMap::new()),
            next_submit_order: AtomicU64::new(1),
            retention,
        }
    }

    /// Store a new task and assign a monotonically increasing submit order.
    ///
    /// Inputs:
    /// - `task`: pending task created from the request.
    pub async fn submit(&self, mut task: ParseTask) -> ParseTask {
        task.submit_order = self.next_submit_order.fetch_add(1, Ordering::SeqCst);
        let notify = Arc::new(Notify::new());
        let active_result_readers = Arc::new(AtomicUsize::new(0));
        let snapshot = task.clone();
        self.tasks.write().await.insert(
            task.task_id,
            TaskEntry {
                task,
                notify,
                active_result_readers,
            },
        );
        snapshot
    }

    pub async fn get(&self, task_id: Uuid) -> Option<ParseTask> {
        self.tasks
            .read()
            .await
            .get(&task_id)
            .map(|entry| entry.task.clone())
    }

    pub async fn set_processing(&self, task_id: Uuid) {
        if let Some(entry) = self.tasks.write().await.get_mut(&task_id) {
            entry.task.status = TaskStatus::Processing;
            entry.task.started_at = Some(Utc::now());
            entry.task.error = None;
            entry.notify.notify_waiters();
        }
    }

    pub async fn set_completed(&self, task_id: Uuid, file_names: Vec<String>) {
        if let Some(entry) = self.tasks.write().await.get_mut(&task_id) {
            entry.task.status = TaskStatus::Completed;
            entry.task.file_names = file_names;
            entry.task.completed_at = Some(Utc::now());
            entry.notify.notify_waiters();
        }
    }

    pub async fn set_failed(&self, task_id: Uuid, error: String) {
        if let Some(entry) = self.tasks.write().await.get_mut(&task_id) {
            entry.task.status = TaskStatus::Failed;
            entry.task.error = Some(error);
            entry.task.completed_at = Some(Utc::now());
            entry.notify.notify_waiters();
        }
    }

    pub async fn wait_terminal(&self, task_id: Uuid) -> Option<ParseTask> {
        loop {
            let notify = {
                let tasks = self.tasks.read().await;
                let entry = tasks.get(&task_id)?;
                if entry.task.status.is_terminal() {
                    return Some(entry.task.clone());
                }
                entry.notify.clone()
            };
            notify.notified().await;
        }
    }

    pub async fn queued_ahead(&self, task_id: Uuid) -> Option<usize> {
        let tasks = self.tasks.read().await;
        let task = tasks.get(&task_id)?;
        if task.task.status != TaskStatus::Pending {
            return Some(0);
        }
        Some(
            tasks
                .values()
                .filter(|entry| {
                    entry.task.task_id != task_id
                        && entry.task.status == TaskStatus::Pending
                        && entry.task.submit_order > 0
                        && entry.task.submit_order < task.task.submit_order
                })
                .count(),
        )
    }

    pub async fn stats(&self) -> TaskStats {
        let mut stats = TaskStats::default();
        for entry in self.tasks.read().await.values() {
            match entry.task.status {
                TaskStatus::Pending => stats.pending += 1,
                TaskStatus::Processing => stats.processing += 1,
                TaskStatus::Completed => stats.completed += 1,
                TaskStatus::Failed => stats.failed += 1,
            }
        }
        stats
    }

    pub async fn acquire_result_lease(&self, task_id: Uuid) -> Option<ResultReadLease> {
        let tasks = self.tasks.read().await;
        let entry = tasks.get(&task_id)?;
        entry.active_result_readers.fetch_add(1, Ordering::SeqCst);
        Some(ResultReadLease {
            active_result_readers: entry.active_result_readers.clone(),
        })
    }

    pub async fn status_payload(&self, task: &ParseTask, base_url: &str) -> StatusPayload {
        StatusPayload {
            task_id: task.task_id.to_string(),
            status: task.status.as_str().to_string(),
            backend: task.backend.clone(),
            file_names: task.file_names.clone(),
            created_at: task.created_at.to_rfc3339(),
            started_at: task.started_at.map(|value| value.to_rfc3339()),
            completed_at: task.completed_at.map(|value| value.to_rfc3339()),
            error: task.error.clone(),
            status_url: format!("{base_url}/tasks/{}", task.task_id),
            result_url: format!("{base_url}/tasks/{}/result", task.task_id),
            queued_ahead: self.queued_ahead(task.task_id).await,
        }
    }

    pub async fn cleanup_expired(&self) -> Vec<PathBuf> {
        if self.retention.is_zero() {
            return Vec::new();
        }
        let now = Utc::now();
        let mut expired = Vec::new();
        let mut tasks = self.tasks.write().await;
        let expired_ids: Vec<Uuid> = tasks
            .iter()
            .filter_map(|(task_id, entry)| {
                if !entry.task.status.is_terminal() {
                    return None;
                }
                if entry.active_result_readers.load(Ordering::SeqCst) > 0 {
                    return None;
                }
                let completed_at = entry.task.completed_at?;
                let age = now.signed_duration_since(completed_at).to_std().ok()?;
                (age >= self.retention).then_some(*task_id)
            })
            .collect();
        for task_id in expired_ids {
            if let Some(entry) = tasks.remove(&task_id) {
                entry.notify.notify_waiters();
                expired.push(entry.task.output_dir);
            }
        }
        expired
    }
}
