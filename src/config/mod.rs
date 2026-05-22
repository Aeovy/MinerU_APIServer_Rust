use std::{env, net::IpAddr, path::PathBuf, str::FromStr, time::Duration};

use clap::Parser;

pub const API_PROTOCOL_VERSION: u8 = 1;
pub const MINERU_VERSION: &str = "3.1.15";
pub const DEFAULT_MAX_CONCURRENT_REQUESTS: usize = 3;
pub const DEFAULT_PROCESSING_WINDOW_SIZE: usize = 64;
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
    pub processing_window_size: usize,
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
        let processing_window_size = read_usize_env(
            "MINERU_PROCESSING_WINDOW_SIZE",
            DEFAULT_PROCESSING_WINDOW_SIZE,
            1,
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
            processing_window_size,
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
    env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value >= minimum)
        .unwrap_or(default)
}

fn read_u64_env(name: &str, default: u64, minimum: u64) -> u64 {
    env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value >= minimum)
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::is_public_bind_host;

    #[test]
    fn detects_public_bind_hosts() {
        assert!(is_public_bind_host("0.0.0.0"));
        assert!(is_public_bind_host("::"));
        assert!(!is_public_bind_host("127.0.0.1"));
        assert!(!is_public_bind_host("localhost"));
    }
}
