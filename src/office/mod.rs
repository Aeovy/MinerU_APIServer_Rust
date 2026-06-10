pub mod docx;
pub mod image_serializer;
pub mod inline;
pub mod markdown;
pub mod model;
pub mod package;
pub mod pptx;
pub mod rels;
pub mod writer_adapter;
pub mod xlsx;

use crate::{
    domain::models::{ParseTask, ParsedDocument, StoredUpload},
    error::{ApiError, ApiResult},
};

#[derive(Clone, Default)]
pub struct OfficeDocumentParser;

impl OfficeDocumentParser {
    pub fn new() -> Self {
        Self
    }

    /// Parse one Office OOXML upload into a MinerU-compatible document.
    ///
    /// Inputs:
    /// - `task`: task options and output directory.
    /// - `upload`: persisted Office file metadata.
    pub async fn parse_upload(
        &self,
        task: &ParseTask,
        upload: &StoredUpload,
    ) -> ApiResult<ParsedDocument> {
        match upload.suffix.as_str() {
            "docx" => docx::parse_docx(task, upload).await,
            "pptx" => pptx::parse_pptx(task, upload).await,
            "xlsx" => xlsx::parse_xlsx(task, upload).await,
            "doc" | "ppt" | "xls" => Err(ApiError::BadRequest(format!(
                "Unsupported legacy Office binary format: {}",
                upload.suffix
            ))),
            other => Err(ApiError::BadRequest(format!(
                "Unsupported Office OOXML file type: {other}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use chrono::Utc;
    use tempfile::tempdir;
    use uuid::Uuid;
    use zip::{write::SimpleFileOptions, ZipWriter};

    use crate::domain::models::{ParseTask, StoredUpload, TaskStatus};

    use super::OfficeDocumentParser;

    #[tokio::test]
    async fn parses_minimal_docx_text_table_and_image() {
        let temp = tempdir().expect("tempdir");
        let upload_path = temp.path().join("sample.docx");
        write_zip(
            &upload_path,
            &[
                (
                    "[Content_Types].xml",
                    br#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#.as_slice(),
                ),
                (
                    "word/document.xml",
                    br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><w:body><w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Title</w:t></w:r></w:p><w:p><w:r><w:t>Hello</w:t></w:r></w:p><w:tbl><w:tr><w:tc><w:p><w:r><w:t>A</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>B</w:t></w:r></w:p></w:tc></w:tr></w:tbl><w:p><w:r><w:drawing><a:blip r:embed="rId1"/></w:drawing></w:r></w:p></w:body></w:document>"#.as_slice(),
                ),
                (
                    "word/_rels/document.xml.rels",
                    br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/image1.png"/></Relationships>"#.as_slice(),
                ),
                ("word/media/image1.png", png_bytes()),
            ],
        );
        let task = task_for_upload(temp.path().to_path_buf(), &upload_path, "docx");
        let parser = OfficeDocumentParser::new();
        let document = parser
            .parse_upload(
                &task,
                &StoredUpload {
                    stem: "sample".to_string(),
                    path: upload_path,
                    suffix: "docx".to_string(),
                },
            )
            .await
            .expect("docx should parse");

        assert_eq!(document.middle_json["_backend"], "office");
        assert!(document.markdown.contains("# Title"));
        assert!(document.markdown.contains("<table>"));
        assert_eq!(document.image_files.len(), 1);
    }

    #[tokio::test]
    async fn parses_docx_inline_spans_and_records_skipped_images() {
        let temp = tempdir().expect("tempdir");
        let upload_path = temp.path().join("inline.docx");
        write_zip(
            &upload_path,
            &[
                (
                    "[Content_Types].xml",
                    br#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#.as_slice(),
                ),
                (
                    "word/document.xml",
                    br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><w:body><w:p><w:r><w:b/><w:t>Bold</w:t></w:r><w:r><w:t> and </w:t></w:r><w:hyperlink r:id="rLink"><w:r><w:u/><w:t>Link</w:t></w:r></w:hyperlink><m:oMath xmlns:m="http://schemas.openxmlformats.org/officeDocument/2006/math"><m:r><m:t>x+1</m:t></m:r></m:oMath></w:p><w:p><w:r><w:drawing><a:blip r:embed="rMissing"/></w:drawing></w:r></w:p></w:body></w:document>"#.as_slice(),
                ),
                (
                    "word/_rels/document.xml.rels",
                    br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rLink" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://example.com" TargetMode="External"/><Relationship Id="rMissing" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/missing.png"/></Relationships>"#.as_slice(),
                ),
            ],
        );
        let task = task_for_upload(temp.path().to_path_buf(), &upload_path, "docx");
        let parser = OfficeDocumentParser::new();
        let document = parser
            .parse_upload(
                &task,
                &StoredUpload {
                    stem: "inline".to_string(),
                    path: upload_path,
                    suffix: "docx".to_string(),
                },
            )
            .await
            .expect("docx should parse");

        let spans = &document.middle_json["pdf_info"][0]["para_blocks"][0]["lines"][0]["spans"];
        assert!(spans
            .as_array()
            .expect("spans should be an array")
            .iter()
            .any(|span| span["type"] == "hyperlink" && span["url"] == "https://example.com"));
        assert!(spans
            .as_array()
            .expect("spans should be an array")
            .iter()
            .any(|span| span["type"] == "inline_equation" && span["content"] == "x+1"));
        assert!(document.markdown.contains("**Bold**"));
        assert!(document
            .markdown
            .contains("[<u>Link</u>](https://example.com)"));
        assert!(document.markdown.contains("$x+1$"));
        assert_eq!(
            document.middle_json["pdf_info"][0]["discarded_blocks"][0]["reason"],
            "missing_image_part"
        );
        assert!(document.model_output["warnings"][0]
            .as_str()
            .expect("warning should be string")
            .contains("missing_image_part"));
    }

    #[tokio::test]
    async fn parses_docx_styles_outline_lists_and_vector_placeholders() {
        let temp = tempdir().expect("tempdir");
        let upload_path = temp.path().join("styled.docx");
        write_zip(
            &upload_path,
            &[
                (
                    "[Content_Types].xml",
                    br#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="emf" ContentType="image/x-emf"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/><Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/></Types>"#.as_slice(),
                ),
                (
                    "word/styles.xml",
                    br#"<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:style w:type="paragraph" w:styleId="Heading1"><w:name w:val="heading 1"/></w:style><w:style w:type="paragraph" w:styleId="CustomTitle"><w:name w:val="Custom Title"/><w:basedOn w:val="Heading1"/></w:style><w:style w:type="paragraph" w:styleId="Outline3"><w:pPr><w:outlineLvl w:val="2"/></w:pPr></w:style></w:styles>"#.as_slice(),
                ),
                (
                    "word/document.xml",
                    br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><w:body><w:p><w:pPr><w:pStyle w:val="CustomTitle"/><w:numPr><w:ilvl w:val="0"/></w:numPr></w:pPr><w:r><w:t>Custom Heading</w:t></w:r></w:p><w:p><w:pPr><w:pStyle w:val="Outline3"/></w:pPr><w:r><w:t>Outline Heading</w:t></w:r></w:p><w:p><w:pPr><w:numPr><w:ilvl w:val="0"/></w:numPr></w:pPr><w:r><w:t>List Item</w:t></w:r></w:p><w:p><w:r><w:drawing><a:blip r:embed="rEmf"/></w:drawing></w:r></w:p></w:body></w:document>"#.as_slice(),
                ),
                (
                    "word/_rels/document.xml.rels",
                    br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rEmf" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/image1.emf"/></Relationships>"#.as_slice(),
                ),
                ("word/media/image1.emf", b"emf-bytes".as_slice()),
            ],
        );
        let task = task_for_upload(temp.path().to_path_buf(), &upload_path, "docx");
        let parser = OfficeDocumentParser::new();
        let document = parser
            .parse_upload(
                &task,
                &StoredUpload {
                    stem: "styled".to_string(),
                    path: upload_path,
                    suffix: "docx".to_string(),
                },
            )
            .await
            .expect("docx should parse");

        assert!(document.markdown.contains("# Custom Heading"));
        assert!(document.markdown.contains("### Outline Heading"));
        assert!(document.markdown.contains("- List Item"));
        assert!(!document.markdown.contains(".emf"));
        assert!(document.markdown.contains("](images/"));
        assert!(document
            .content_list
            .as_array()
            .expect("content list should be an array")
            .iter()
            .any(|item| item["type"] == "text"
                && item["text"] == "Custom Heading"
                && item["text_level"] == 1));
        assert!(document
            .content_list
            .as_array()
            .expect("content list should be an array")
            .iter()
            .any(|item| item["type"] == "list"
                && item["list_items"][0] == "List Item"
                && item["page_idx"] == 0));
        assert!(document
            .content_list
            .as_array()
            .expect("content list should be an array")
            .iter()
            .any(|item| item["type"] == "image"
                && item["img_path"]
                    .as_str()
                    .expect("image path should be string")
                    .starts_with("images/")
                && item["page_idx"] == 0));
        let first_page = document.content_list_v2[0]
            .as_array()
            .expect("content list v2 page should be an array");
        assert!(first_page.iter().any(|item| item["type"] == "title"
            && item["content"]["level"] == 1
            && item["content"]["title_content"][0]["content"] == "Custom Heading"));
        assert!(first_page.iter().any(|item| item["type"] == "list"
            && item["content"]["attribute"] == "unordered"
            && item["content"]["list_items"][0]["prefix"] == "-"
            && item["content"]["list_items"][0]["item_content"][0]["content"] == "List Item"));
        assert!(first_page.iter().any(|item| item["type"] == "image"
            && item["content"]["image_source"]["path"]
                .as_str()
                .expect("v2 image path should be string")
                .starts_with("images/")));
        assert_eq!(document.image_files.len(), 1);
        let image_name = document.image_files[0]
            .file_name()
            .and_then(|name| name.to_str())
            .expect("image file name");
        assert!(image_name.ends_with(".jpg"));
        assert!(!image_name.contains("image1"));
        assert!(document.model_output["warnings"][0]
            .as_str()
            .expect("warning should be string")
            .contains("EMF"));
    }

    #[tokio::test]
    async fn parses_pptx_slide_order_and_notes() {
        let temp = tempdir().expect("tempdir");
        let upload_path = temp.path().join("slides.pptx");
        write_zip(
            &upload_path,
            &[
                (
                    "[Content_Types].xml",
                    br#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/></Types>"#.as_slice(),
                ),
                (
                    "ppt/presentation.xml",
                    br#"<p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><p:sldIdLst><p:sldId id="2" r:id="rId2"/><p:sldId id="1" r:id="rId1"/></p:sldIdLst></p:presentation>"#.as_slice(),
                ),
                (
                    "ppt/_rels/presentation.xml.rels",
                    br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide2.xml"/></Relationships>"#.as_slice(),
                ),
                (
                    "ppt/slides/slide1.xml",
                    br#"<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>First file second order</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#.as_slice(),
                ),
                (
                    "ppt/slides/slide2.xml",
                    br#"<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>Second file first order</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#.as_slice(),
                ),
                (
                    "ppt/slides/_rels/slide2.xml.rels",
                    br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rIdNotes" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesSlide" Target="../notesSlides/notesSlide2.xml"/></Relationships>"#.as_slice(),
                ),
                (
                    "ppt/notesSlides/notesSlide2.xml",
                    br#"<p:notes xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>speaker note</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:notes>"#.as_slice(),
                ),
            ],
        );
        let task = task_for_upload(temp.path().to_path_buf(), &upload_path, "pptx");
        let parser = OfficeDocumentParser::new();
        let document = parser
            .parse_upload(
                &task,
                &StoredUpload {
                    stem: "slides".to_string(),
                    path: upload_path,
                    suffix: "pptx".to_string(),
                },
            )
            .await
            .expect("pptx should parse");

        assert!(document.markdown.contains("Second file first order"));
        assert!(document.markdown.contains("speaker note"));
        assert_eq!(
            document.middle_json["pdf_info"][0]["page_idx"],
            serde_json::json!(0)
        );
    }

    #[tokio::test]
    async fn parses_pptx_xy_order_and_filters_decorative_images() {
        let temp = tempdir().expect("tempdir");
        let upload_path = temp.path().join("layout.pptx");
        write_zip(
            &upload_path,
            &[
                (
                    "[Content_Types].xml",
                    br#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/></Types>"#.as_slice(),
                ),
                (
                    "ppt/presentation.xml",
                    br#"<p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><p:sldSz cx="9144000" cy="5143500"/><p:sldIdLst><p:sldId id="1" r:id="rId1"/></p:sldIdLst></p:presentation>"#.as_slice(),
                ),
                (
                    "ppt/_rels/presentation.xml.rels",
                    br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/></Relationships>"#.as_slice(),
                ),
                (
                    "ppt/slides/slide1.xml",
                    br#"<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><p:cSld><p:spTree><p:sp><p:spPr><a:xfrm><a:off x="100000" y="2000000"/><a:ext cx="2000000" cy="500000"/></a:xfrm></p:spPr><p:txBody><a:p><a:r><a:t>Lower text</a:t></a:r></a:p></p:txBody></p:sp><p:sp><p:spPr><a:xfrm><a:off x="100000" y="100000"/><a:ext cx="2000000" cy="500000"/></a:xfrm></p:spPr><p:txBody><a:p><a:r><a:t>Upper text</a:t></a:r></a:p></p:txBody></p:sp><p:pic><p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="9144000" cy="5143500"/></a:xfrm></p:spPr><p:blipFill><a:blip r:embed="rBg"/></p:blipFill></p:pic><p:pic><p:spPr><a:xfrm><a:off x="500000" y="500000"/><a:ext cx="50000" cy="50000"/></a:xfrm></p:spPr><p:blipFill><a:blip r:embed="rTiny"/></p:blipFill></p:pic></p:spTree></p:cSld></p:sld>"#.as_slice(),
                ),
                (
                    "ppt/slides/_rels/slide1.xml.rels",
                    br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rBg" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/background.png"/><Relationship Id="rTiny" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/tiny.png"/></Relationships>"#.as_slice(),
                ),
                ("ppt/media/background.png", png_bytes()),
                ("ppt/media/tiny.png", png_bytes()),
            ],
        );
        let task = task_for_upload(temp.path().to_path_buf(), &upload_path, "pptx");
        let parser = OfficeDocumentParser::new();
        let document = parser
            .parse_upload(
                &task,
                &StoredUpload {
                    stem: "layout".to_string(),
                    path: upload_path,
                    suffix: "pptx".to_string(),
                },
            )
            .await
            .expect("pptx should parse");

        let upper_position = document
            .markdown
            .find("Upper text")
            .expect("upper text should be emitted");
        let lower_position = document
            .markdown
            .find("Lower text")
            .expect("lower text should be emitted");
        assert!(upper_position < lower_position);
        assert!(document.image_files.is_empty());
    }

    #[tokio::test]
    async fn parses_xlsx_visible_sheets_and_skips_hidden() {
        let temp = tempdir().expect("tempdir");
        let upload_path = temp.path().join("book.xlsx");
        write_zip(
            &upload_path,
            &[
                (
                    "[Content_Types].xml",
                    br#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/worksheets/sheet2.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#.as_slice(),
                ),
                (
                    "_rels/.rels",
                    br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rIdWorkbook" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#.as_slice(),
                ),
                (
                    "xl/workbook.xml",
                    br#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Visible" sheetId="1" r:id="rId1"/><sheet name="Hidden" sheetId="2" state="hidden" r:id="rId2"/></sheets></workbook>"#.as_slice(),
                ),
                (
                    "xl/_rels/workbook.xml.rels",
                    br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet2.xml"/></Relationships>"#.as_slice(),
                ),
                (
                    "xl/worksheets/sheet1.xml",
                    br#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>Merged Header</t></is></c><c r="B1" t="inlineStr"><is><t></t></is></c></row><row r="2"><c r="A2" t="inlineStr"><is><t>alpha</t></is></c><c r="B2"><v>42</v></c></row></sheetData><mergeCells count="1"><mergeCell ref="A1:B1"/></mergeCells></worksheet>"#.as_slice(),
                ),
                (
                    "xl/worksheets/sheet2.xml",
                    br#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>hidden text</t></is></c></row></sheetData></worksheet>"#.as_slice(),
                ),
            ],
        );
        let task = task_for_upload(temp.path().to_path_buf(), &upload_path, "xlsx");
        let parser = OfficeDocumentParser::new();
        let document = parser
            .parse_upload(
                &task,
                &StoredUpload {
                    stem: "book".to_string(),
                    path: upload_path,
                    suffix: "xlsx".to_string(),
                },
            )
            .await
            .expect("xlsx should parse");

        assert!(document.markdown.contains("alpha"));
        assert!(document.markdown.contains("<table>"));
        assert!(document.markdown.contains(r#"colspan="2""#));
        assert!(!document.markdown.contains("hidden text"));
    }

    fn write_zip(path: &std::path::Path, entries: &[(&str, &[u8])]) {
        let file = std::fs::File::create(path).expect("zip file");
        let mut writer = ZipWriter::new(file);
        let options = SimpleFileOptions::default();
        for (name, bytes) in entries {
            writer.start_file(name, options).expect("zip entry");
            writer.write_all(bytes).expect("zip bytes");
        }
        writer.finish().expect("finish zip");
    }

    fn task_for_upload(
        output_dir: std::path::PathBuf,
        upload_path: &std::path::Path,
        suffix: &str,
    ) -> ParseTask {
        ParseTask {
            task_id: Uuid::new_v4(),
            status: TaskStatus::Processing,
            backend: "vlm-http-client".to_string(),
            file_names: vec!["sample".to_string()],
            created_at: Utc::now(),
            output_dir,
            image_analysis: true,
            server_url: None,
            return_md: true,
            return_middle_json: true,
            return_model_output: true,
            return_content_list: true,
            return_images: true,
            response_format_zip: false,
            return_original_file: false,
            start_page_id: 0,
            end_page_id: 99999,
            uploads: vec![upload_path.to_path_buf()],
            upload_suffixes: vec![suffix.to_string()],
            submit_order: 0,
            started_at: Some(Utc::now()),
            completed_at: None,
            error: None,
        }
    }

    fn png_bytes() -> &'static [u8] {
        &[
            137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1,
            8, 6, 0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 156, 99, 248, 15, 4,
            0, 9, 251, 3, 253, 160, 152, 198, 53, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
        ]
    }
}
