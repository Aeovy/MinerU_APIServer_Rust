use utoipa::OpenApi;

#[derive(OpenApi)]
#[openapi(
    paths(
        crate::server::routes::file_parse,
        crate::server::routes::submit_task,
        crate::server::routes::get_task_status,
        crate::server::routes::get_task_result,
        crate::server::routes::health
    ),
    components(schemas(
        crate::domain::models::StatusPayload,
        crate::domain::models::HealthPayload,
        crate::domain::models::TaskStatus
    )),
    tags((name = "mineru", description = "MinerU-compatible vlm-http-client API"))
)]
pub struct ApiDoc;
