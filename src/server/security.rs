use crate::{
    config::PUBLIC_BIND_DISABLED_DETAIL,
    domain::models::ParseOptions,
    error::{ApiError, ApiResult},
};

/// Validate MinerU's public-bind protection for caller-supplied remote inference URLs.
///
/// Inputs:
/// - `public_bind_exposed`: whether the API listens on a public wildcard host.
/// - `allow_public_http_client`: whether the user explicitly accepted the SSRF risk.
/// - `options`: parsed request options containing backend and server URL.
pub fn validate_public_http_client_policy(
    public_bind_exposed: bool,
    allow_public_http_client: bool,
    options: &ParseOptions,
) -> ApiResult<()> {
    if !public_bind_exposed || allow_public_http_client {
        return Ok(());
    }
    if options.backend.ends_with("-http-client") || options.server_url_has_value() {
        return Err(ApiError::BadRequest(
            PUBLIC_BIND_DISABLED_DETAIL.to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::domain::models::ParseOptions;

    use super::validate_public_http_client_policy;

    #[test]
    fn rejects_http_client_when_public_without_override() {
        let options = ParseOptions {
            backend: "vlm-http-client".to_string(),
            ..ParseOptions::default()
        };
        assert!(validate_public_http_client_policy(true, false, &options).is_err());
        assert!(validate_public_http_client_policy(true, true, &options).is_ok());
        assert!(validate_public_http_client_policy(false, false, &options).is_ok());
    }
}
