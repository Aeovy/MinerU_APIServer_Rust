use serde_json::json;
use utoipa::openapi::{
    content::ContentBuilder,
    encoding::EncodingBuilder,
    path::ParameterStyle,
    request_body::RequestBodyBuilder,
    schema::{ArrayBuilder, KnownFormat, ObjectBuilder, SchemaFormat, SchemaType, Type},
    Required,
};
use utoipa::{Modify, OpenApi};

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
    modifiers(&MultipartSchema),
    tags((name = "mineru", description = "MinerU-compatible vlm-http-client API"))
)]
pub struct ApiDoc;

pub struct MultipartSchema;

impl Modify for MultipartSchema {
    /// Add MinerU-compatible multipart form schemas for clients generated from OpenAPI.
    ///
    /// Inputs:
    /// - `openapi`: generated OpenAPI document before it is served.
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        set_multipart_schema(openapi, "/file_parse");
        set_multipart_schema(openapi, "/tasks");
    }
}

/// Replace utoipa's empty multipart schema with MinerU-compatible form fields.
///
/// Inputs:
/// - `openapi`: generated OpenAPI document to patch.
/// - `path`: API path whose POST operation accepts multipart form data.
fn set_multipart_schema(openapi: &mut utoipa::openapi::OpenApi, path: &str) {
    let Some(operation) = openapi
        .paths
        .paths
        .get_mut(path)
        .and_then(|path_item| path_item.post.as_mut())
    else {
        return;
    };

    let form_encoding = EncodingBuilder::new()
        .style(Some(ParameterStyle::Form))
        .explode(Some(true))
        .build();

    operation.request_body = Some(
        RequestBodyBuilder::new()
            .required(Some(Required::True))
            .content(
                "multipart/form-data",
                ContentBuilder::new()
                    .schema(Some(multipart_form_schema()))
                    .encoding("files", form_encoding.clone())
                    .encoding("lang_list", form_encoding)
                    .build(),
            )
            .build(),
    );
}

/// Build the documented request body object used by `/file_parse` and `/tasks`.
fn multipart_form_schema() -> ObjectBuilder {
    ObjectBuilder::new()
        .schema_type(Type::Object)
        .required("files")
        .property(
            "files",
            ArrayBuilder::new()
                .items(
                    ObjectBuilder::new()
                        .schema_type(Type::String)
                        .format(Some(SchemaFormat::KnownFormat(KnownFormat::Binary))),
                )
                .description(Some(
                    "Upload PDF, image, or Office OOXML files (docx, pptx, xlsx) for parsing",
                )),
        )
        .property(
            "lang_list",
            ArrayBuilder::new()
                .items(ObjectBuilder::new().schema_type(Type::String))
                .default(Some(json!(["ch"]))),
        )
        .property(
            "backend",
            ObjectBuilder::new()
                .schema_type(Type::String)
                .default(Some(json!("vlm-http-client")))
                .enum_values(Some(["vlm-http-client", "vllm-http-client"])),
        )
        .property(
            "parse_method",
            ObjectBuilder::new()
                .schema_type(Type::String)
                .default(Some(json!("auto")))
                .enum_values(Some(["auto", "txt", "ocr"])),
        )
        .property("formula_enable", boolean_schema(true))
        .property("table_enable", boolean_schema(true))
        .property("image_analysis", boolean_schema(true))
        .property(
            "server_url",
            ObjectBuilder::new()
                .schema_type(SchemaType::from_iter([Type::String, Type::Null]))
                .description(Some("OpenAI-compatible VLM server URL")),
        )
        .property("return_md", boolean_schema(true))
        .property("return_middle_json", boolean_schema(false))
        .property("return_model_output", boolean_schema(false))
        .property("return_content_list", boolean_schema(false))
        .property("return_images", boolean_schema(false))
        .property("response_format_zip", boolean_schema(false))
        .property("return_original_file", boolean_schema(false))
        .property("start_page_id", page_id_schema(0))
        .property("end_page_id", page_id_schema(99999))
}

/// Build a boolean schema with the MinerU-compatible default value.
///
/// Inputs:
/// - `default`: default value shown to OpenAPI clients and Swagger UI.
fn boolean_schema(default: bool) -> ObjectBuilder {
    ObjectBuilder::new()
        .schema_type(Type::Boolean)
        .default(Some(json!(default)))
}

/// Build a non-negative page id schema with the provided default.
///
/// Inputs:
/// - `default`: default page index used when the form field is omitted.
fn page_id_schema(default: i32) -> ObjectBuilder {
    ObjectBuilder::new()
        .schema_type(Type::Integer)
        .default(Some(json!(default)))
        .minimum(Some(0))
}

#[cfg(test)]
mod tests {
    use utoipa::OpenApi;

    use super::ApiDoc;

    #[test]
    fn multipart_schema_documents_office_ooxml_uploads() {
        let value = serde_json::to_value(ApiDoc::openapi()).expect("openapi serializes");
        let description = value["paths"]["/file_parse"]["post"]["requestBody"]["content"]
            ["multipart/form-data"]["schema"]["properties"]["files"]["description"]
            .as_str()
            .expect("files description should exist");
        assert!(description.contains("docx"));
        assert!(description.contains("pptx"));
        assert!(description.contains("xlsx"));
    }
}
