use std::{env, net::IpAddr, path::PathBuf, str::FromStr, thread, time::Duration};

use clap::Parser;

pub const API_PROTOCOL_VERSION: u8 = 1;
pub const MINERU_VERSION: &str = "3.1.15";
pub const DEFAULT_MAX_CONCURRENT_REQUESTS: usize = 3;
pub const DEFAULT_MAX_UPLOAD_SIZE_BYTES: usize = 256 * 1024 * 1024;
pub const DEFAULT_PROCESSING_WINDOW_SIZE: usize = 8;
pub const DEFAULT_VLM_MAX_CONCURRENCY: usize = 50;
pub const DEFAULT_VLM_QUEUE_CAPACITY_MULTIPLIER: usize = 4;
pub const DEFAULT_VLM_MAX_REQUESTS_PER_TASK_CEILING: usize = 8;
pub const DEFAULT_IMAGE_PREPROCESS_THREADS: usize = 0;
pub const DEFAULT_MEMORY_RECLAIM_AFTER_TASK: bool = true;
pub const DEFAULT_TASK_RETENTION_SECONDS: u64 = 24 * 60 * 60;
pub const DEFAULT_TASK_CLEANUP_INTERVAL_SECONDS: u64 = 5 * 60;
pub const DEFAULT_OUTPUT_ROOT: &str = "./output";
pub const PUBLIC_BIND_DISABLED_DETAIL: &str = "Publicly exposed API disables *-http-client backends and server_url by default. Rebind to 127.0.0.1 or start with --allow-public-http-client if you understand the SSRF risk.";

#[derive(Debug, Clone, Parser)]
#[command(
    name = "mineru-api",
    about = "MinerU-compatible Rust API service for vlm-http-client"
)]
pub struct CliArgs {
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,
    #[arg(long, default_value_t = 8000)]
    pub port: u16,
    #[arg(long, default_value_t = false)]
    pub allow_public_http_client: bool,
}

#[derive(Debug, Clone)]
pub struct ServiceConfig {
    pub public_bind_exposed: bool,
    pub allow_public_http_client: bool,
    pub output_root: PathBuf,
    pub max_concurrent_requests: usize,
    pub max_in_flight_tasks: usize,
    pub max_upload_size_bytes: usize,
    pub processing_window_size: usize,
    pub vlm_max_concurrency: usize,
    pub vlm_queue_capacity: usize,
    pub max_vlm_requests_per_task: usize,
    pub image_preprocess_threads: usize,
    pub memory_reclaim_after_task: bool,
    pub task_retention: Duration,
    pub task_cleanup_interval: Duration,
}

impl ServiceConfig {
    /// Build runtime service configuration from CLI arguments and MinerU-compatible environment variables.
    ///
    /// Inputs:
    /// - `args`: command-line options provided by the user.
    pub fn from_args(args: &CliArgs) -> Self {
        let default_max_concurrent_requests = if cfg!(target_os = "macos") {
            1
        } else {
            DEFAULT_MAX_CONCURRENT_REQUESTS
        };
        let max_concurrent_requests = read_usize_env(
            "MINERU_API_MAX_CONCURRENT_REQUESTS",
            default_max_concurrent_requests,
            1,
        );
        let max_in_flight_tasks = read_usize_env(
            "MINERU_API_MAX_IN_FLIGHT_TASKS",
            max_concurrent_requests.saturating_mul(2).max(1),
            1,
        );
        let max_upload_size_bytes = read_usize_env(
            "MINERU_API_MAX_UPLOAD_SIZE_BYTES",
            DEFAULT_MAX_UPLOAD_SIZE_BYTES,
            1,
        );
        let processing_window_size = read_usize_env(
            "MINERU_PROCESSING_WINDOW_SIZE",
            DEFAULT_PROCESSING_WINDOW_SIZE,
            1,
        );
        let vlm_max_concurrency =
            read_usize_env("MINERU_VLM_MAX_CONCURRENCY", DEFAULT_VLM_MAX_CONCURRENCY, 1);
        let vlm_queue_capacity = read_usize_env(
            "MINERU_VLM_QUEUE_CAPACITY",
            vlm_max_concurrency
                .saturating_mul(DEFAULT_VLM_QUEUE_CAPACITY_MULTIPLIER)
                .max(1),
            1,
        );
        let max_vlm_requests_per_task = read_usize_env(
            "MINERU_VLM_MAX_REQUESTS_PER_TASK",
            vlm_max_concurrency.clamp(1, DEFAULT_VLM_MAX_REQUESTS_PER_TASK_CEILING),
            1,
        )
        .min(vlm_max_concurrency)
        .max(1);
        let image_preprocess_threads = read_usize_env(
            "MINERU_IMAGE_PREPROCESS_THREADS",
            default_image_preprocess_threads(),
            1,
        );
        let memory_reclaim_after_task = read_bool_env(
            "MINERU_MEMORY_RECLAIM_AFTER_TASK",
            DEFAULT_MEMORY_RECLAIM_AFTER_TASK,
        );
        let task_retention = Duration::from_secs(read_u64_env(
            "MINERU_API_TASK_RETENTION_SECONDS",
            DEFAULT_TASK_RETENTION_SECONDS,
            0,
        ));
        let task_cleanup_interval = Duration::from_secs(read_u64_env(
            "MINERU_API_TASK_CLEANUP_INTERVAL_SECONDS",
            DEFAULT_TASK_CLEANUP_INTERVAL_SECONDS,
            1,
        ));
        let output_root = env::var("MINERU_API_OUTPUT_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_OUTPUT_ROOT));

        Self {
            public_bind_exposed: is_public_bind_host(&args.host),
            allow_public_http_client: args.allow_public_http_client,
            output_root,
            max_concurrent_requests,
            max_in_flight_tasks,
            max_upload_size_bytes,
            processing_window_size,
            vlm_max_concurrency,
            vlm_queue_capacity,
            max_vlm_requests_per_task,
            image_preprocess_threads,
            memory_reclaim_after_task,
            task_retention,
            task_cleanup_interval,
        }
    }
}

/// Determine whether an API bind host is externally reachable by default.
///
/// Inputs:
/// - `host`: host string passed on the CLI.
pub fn is_public_bind_host(host: &str) -> bool {
    if matches!(host, "0.0.0.0" | "::") {
        return true;
    }
    IpAddr::from_str(host)
        .map(|addr| addr.is_unspecified())
        .unwrap_or(false)
}

fn read_usize_env(name: &str, default: usize, minimum: usize) -> usize {
    read_parsed_env(name, default, minimum)
}

fn read_u64_env(name: &str, default: u64, minimum: u64) -> u64 {
    read_parsed_env(name, default, minimum)
}

fn read_bool_env(name: &str, default: bool) -> bool {
    let Ok(raw) = env::var(name) else {
        return default;
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => true,
        "false" | "0" | "no" | "off" => false,
        value => {
            tracing::warn!(
                name,
                value,
                default,
                "invalid boolean environment value; using default"
            );
            default
        }
    }
}

fn read_parsed_env<T>(name: &str, default: T, minimum: T) -> T
where
    T: Copy + Ord + FromStr + std::fmt::Display,
{
    let Ok(raw) = env::var(name) else {
        return default;
    };
    let trimmed = raw.trim();
    match trimmed.parse::<T>() {
        Ok(value) if value >= minimum => value,
        Ok(value) => {
            tracing::warn!(
                name,
                value = %value,
                minimum = %minimum,
                default = %default,
                "environment value below minimum; using default"
            );
            default
        }
        Err(_) => {
            tracing::warn!(
                name,
                value = %trimmed,
                default = %default,
                "invalid environment value; using default"
            );
            default
        }
    }
}

fn default_image_preprocess_threads() -> usize {
    match DEFAULT_IMAGE_PREPROCESS_THREADS {
        0 => available_parallelism(),
        value => value.max(1),
    }
}

fn available_parallelism() -> usize {
    thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .max(1)
}

#[cfg(test)]
mod tests {
    use super::{
        is_public_bind_host, CliArgs, DEFAULT_MAX_UPLOAD_SIZE_BYTES,
        DEFAULT_MEMORY_RECLAIM_AFTER_TASK, DEFAULT_PROCESSING_WINDOW_SIZE,
        DEFAULT_VLM_MAX_CONCURRENCY, DEFAULT_VLM_MAX_REQUESTS_PER_TASK_CEILING,
        DEFAULT_VLM_QUEUE_CAPACITY_MULTIPLIER,
    };
    use crate::vlm::test_env::{EnvVarGuard, TEST_ENV_LOCK};

    #[test]
    fn detects_public_bind_hosts() {
        assert!(is_public_bind_host("0.0.0.0"));
        assert!(is_public_bind_host("::"));
        assert!(!is_public_bind_host("127.0.0.1"));
        assert!(!is_public_bind_host("localhost"));
    }

    #[tokio::test]
    async fn reads_vlm_max_concurrency_with_default_and_minimum() {
        let _env_lock = TEST_ENV_LOCK.lock().await;
        let args = CliArgs {
            host: "127.0.0.1".to_string(),
            port: 34000,
            allow_public_http_client: false,
        };

        let _value = EnvVarGuard::unset("MINERU_VLM_MAX_CONCURRENCY");
        assert_eq!(
            super::ServiceConfig::from_args(&args).vlm_max_concurrency,
            DEFAULT_VLM_MAX_CONCURRENCY
        );

        let _value = EnvVarGuard::set("MINERU_VLM_MAX_CONCURRENCY", "0");
        assert_eq!(
            super::ServiceConfig::from_args(&args).vlm_max_concurrency,
            DEFAULT_VLM_MAX_CONCURRENCY
        );

        let _value = EnvVarGuard::set("MINERU_VLM_MAX_CONCURRENCY", " 7 ");
        assert_eq!(
            super::ServiceConfig::from_args(&args).vlm_max_concurrency,
            7
        );
    }

    #[tokio::test]
    async fn reads_vlm_scheduler_defaults_and_overrides() {
        let _env_lock = TEST_ENV_LOCK.lock().await;
        let args = CliArgs {
            host: "127.0.0.1".to_string(),
            port: 34000,
            allow_public_http_client: false,
        };

        let _concurrency = EnvVarGuard::set("MINERU_VLM_MAX_CONCURRENCY", "12");
        let _queue = EnvVarGuard::unset("MINERU_VLM_QUEUE_CAPACITY");
        let _task = EnvVarGuard::unset("MINERU_VLM_MAX_REQUESTS_PER_TASK");
        let config = super::ServiceConfig::from_args(&args);
        assert_eq!(
            config.vlm_queue_capacity,
            12 * DEFAULT_VLM_QUEUE_CAPACITY_MULTIPLIER
        );
        assert_eq!(
            config.max_vlm_requests_per_task,
            DEFAULT_VLM_MAX_REQUESTS_PER_TASK_CEILING
        );

        let _queue = EnvVarGuard::set("MINERU_VLM_QUEUE_CAPACITY", "5");
        let _task = EnvVarGuard::set("MINERU_VLM_MAX_REQUESTS_PER_TASK", "99");
        let config = super::ServiceConfig::from_args(&args);
        assert_eq!(config.vlm_queue_capacity, 5);
        assert_eq!(config.max_vlm_requests_per_task, 12);
    }

    #[tokio::test]
    async fn reads_resource_guard_defaults_and_overrides() {
        let _env_lock = TEST_ENV_LOCK.lock().await;
        let args = CliArgs {
            host: "127.0.0.1".to_string(),
            port: 34000,
            allow_public_http_client: false,
        };

        let _window = EnvVarGuard::unset("MINERU_PROCESSING_WINDOW_SIZE");
        let _requests = EnvVarGuard::set("MINERU_API_MAX_CONCURRENT_REQUESTS", "3");
        let _in_flight = EnvVarGuard::unset("MINERU_API_MAX_IN_FLIGHT_TASKS");
        let config = super::ServiceConfig::from_args(&args);
        assert_eq!(
            config.processing_window_size,
            DEFAULT_PROCESSING_WINDOW_SIZE
        );
        assert_eq!(config.max_in_flight_tasks, 6);

        let _window = EnvVarGuard::set("MINERU_PROCESSING_WINDOW_SIZE", "5");
        let _in_flight = EnvVarGuard::set("MINERU_API_MAX_IN_FLIGHT_TASKS", "9");
        let config = super::ServiceConfig::from_args(&args);
        assert_eq!(config.processing_window_size, 5);
        assert_eq!(config.max_in_flight_tasks, 9);

        let _in_flight = EnvVarGuard::set("MINERU_API_MAX_IN_FLIGHT_TASKS", "0");
        assert_eq!(
            super::ServiceConfig::from_args(&args).max_in_flight_tasks,
            6
        );
    }

    #[tokio::test]
    async fn reads_memory_reclaim_after_task_default_and_override() {
        let _env_lock = TEST_ENV_LOCK.lock().await;
        let args = CliArgs {
            host: "127.0.0.1".to_string(),
            port: 34000,
            allow_public_http_client: false,
        };

        let _value = EnvVarGuard::unset("MINERU_MEMORY_RECLAIM_AFTER_TASK");
        assert_eq!(
            super::ServiceConfig::from_args(&args).memory_reclaim_after_task,
            DEFAULT_MEMORY_RECLAIM_AFTER_TASK
        );

        let _value = EnvVarGuard::set("MINERU_MEMORY_RECLAIM_AFTER_TASK", "false");
        assert!(!super::ServiceConfig::from_args(&args).memory_reclaim_after_task);

        let _value = EnvVarGuard::set("MINERU_MEMORY_RECLAIM_AFTER_TASK", "invalid");
        assert_eq!(
            super::ServiceConfig::from_args(&args).memory_reclaim_after_task,
            DEFAULT_MEMORY_RECLAIM_AFTER_TASK
        );
    }

    #[tokio::test]
    async fn reads_max_upload_size_bytes_with_default_and_minimum() {
        let _env_lock = TEST_ENV_LOCK.lock().await;
        let args = CliArgs {
            host: "127.0.0.1".to_string(),
            port: 34000,
            allow_public_http_client: false,
        };

        let _value = EnvVarGuard::unset("MINERU_API_MAX_UPLOAD_SIZE_BYTES");
        assert_eq!(
            super::ServiceConfig::from_args(&args).max_upload_size_bytes,
            DEFAULT_MAX_UPLOAD_SIZE_BYTES
        );

        let _value = EnvVarGuard::set("MINERU_API_MAX_UPLOAD_SIZE_BYTES", "0");
        assert_eq!(
            super::ServiceConfig::from_args(&args).max_upload_size_bytes,
            DEFAULT_MAX_UPLOAD_SIZE_BYTES
        );

        let _value = EnvVarGuard::set("MINERU_API_MAX_UPLOAD_SIZE_BYTES", "4096");
        assert_eq!(
            super::ServiceConfig::from_args(&args).max_upload_size_bytes,
            4096
        );
    }

    #[tokio::test]
    async fn reads_image_preprocess_threads_with_auto_default_and_minimum() {
        let _env_lock = TEST_ENV_LOCK.lock().await;
        let args = CliArgs {
            host: "127.0.0.1".to_string(),
            port: 34000,
            allow_public_http_client: false,
        };

        let _value = EnvVarGuard::unset("MINERU_IMAGE_PREPROCESS_THREADS");
        assert_eq!(
            super::ServiceConfig::from_args(&args).image_preprocess_threads,
            super::available_parallelism()
        );

        let _value = EnvVarGuard::set("MINERU_IMAGE_PREPROCESS_THREADS", "0");
        assert_eq!(
            super::ServiceConfig::from_args(&args).image_preprocess_threads,
            super::available_parallelism()
        );

        let _value = EnvVarGuard::set("MINERU_IMAGE_PREPROCESS_THREADS", "5");
        assert_eq!(
            super::ServiceConfig::from_args(&args).image_preprocess_threads,
            5
        );

        let _value = EnvVarGuard::set("MINERU_IMAGE_PREPROCESS_THREADS", "\"8\"");
        assert_eq!(
            super::ServiceConfig::from_args(&args).image_preprocess_threads,
            super::available_parallelism()
        );
    }
}
