use axum::response::Html;

/// Render the embedded health dashboard page.
///
/// The page is intentionally self-contained so production Docker images can serve it without a
/// frontend build step, CDN access, or extra static-file configuration.
pub(crate) async fn health_page() -> Html<&'static str> {
    Html(HEALTH_PAGE_HTML)
}

const HEALTH_PAGE_HTML: &str = include_str!("health_page.html");

#[cfg(test)]
mod tests {
    use super::{health_page, HEALTH_PAGE_HTML};

    #[tokio::test]
    async fn health_page_serves_embedded_dashboard() {
        let axum::response::Html(body) = health_page().await;

        assert!(body.contains("MinerU Health Dashboard"));
        assert!(body.contains("fetch('/health'"));
        assert!(body.contains("active_vlm_requests"));
        assert!(body.contains("allocator_resident_bytes"));
    }

    #[test]
    fn health_page_has_no_external_runtime_dependencies() {
        assert!(!HEALTH_PAGE_HTML.contains("https://"));
        assert!(!HEALTH_PAGE_HTML.contains("http://"));
        assert!(!HEALTH_PAGE_HTML.contains("<script src="));
        assert!(!HEALTH_PAGE_HTML.contains("<link rel=\"stylesheet\""));
    }
}
