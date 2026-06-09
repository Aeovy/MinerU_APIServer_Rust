pub mod docx;
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
                ("word/media/image1.png", b"png-bytes".as_slice()),
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
                    br#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>Name</t></is></c><c r="B1" t="inlineStr"><is><t>Value</t></is></c></row><row r="2"><c r="A2" t="inlineStr"><is><t>alpha</t></is></c><c r="B2"><v>42</v></c></row></sheetData></worksheet>"#.as_slice(),
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
}
