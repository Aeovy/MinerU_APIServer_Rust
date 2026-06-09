use std::{collections::HashMap, path::Path};

use crate::{
    domain::models::{ParseTask, ParsedDocument, StoredUpload},
    error::{ApiError, ApiResult},
    office::{
        markdown::to_parsed_document,
        model::{OfficeBlock, OfficeDiscardedBlock, OfficeDocument, OfficeImage, OfficePage},
        package::{local_name, OoxmlPackage},
        rels::{read_relationships, relationship_target_part},
        writer_adapter::OfficeMediaWriter,
    },
};
use quick_xml::events::{BytesStart, Event};
use serde_json::json;

const WORD_DOCUMENT_PART: &str = "word/document.xml";
const REL_TYPE_HYPERLINK: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink";

pub async fn parse_docx(task: &ParseTask, upload: &StoredUpload) -> ApiResult<ParsedDocument> {
    let package = OoxmlPackage::open(&upload.path)?;
    if !package.contains(WORD_DOCUMENT_PART) {
        return Err(ApiError::BadRequest(
            "Invalid DOCX package: missing word/document.xml".to_string(),
        ));
    }
    let media_writer = OfficeMediaWriter::new(task, &upload.stem).await?;
    let relationships = read_relationships(&package, WORD_DOCUMENT_PART)?;
    let hyperlink_targets = relationships
        .iter()
        .filter(|rel| rel.rel_type == REL_TYPE_HYPERLINK)
        .map(|rel| (rel.id.clone(), rel.target.clone()))
        .collect::<HashMap<String, String>>();

    let xml = package.read_text(WORD_DOCUMENT_PART)?;
    let mut reader = quick_xml::Reader::from_str(&xml);
    reader.config_mut().trim_text(false);
    let mut blocks = Vec::new();
    let mut images = Vec::new();
    let mut discarded_blocks = Vec::new();
    let mut warnings = Vec::new();
    let mut paragraph = ParagraphState::default();
    let mut table = TableState::default();
    let mut in_paragraph = false;
    let mut in_table = false;
    let mut text_capture = TextCapture::default();
    let mut hyperlink_stack: Vec<Option<String>> = Vec::new();
    let mut run_style = RunStyle::default();

    loop {
        match reader.read_event() {
            Ok(Event::Start(event)) => match local_name(event.name().as_ref()) {
                b"p" => {
                    in_paragraph = true;
                    paragraph = ParagraphState::from_start(&event, &reader)?;
                }
                b"r" => run_style = RunStyle::default(),
                b"pStyle" if in_paragraph => {
                    if let Some(style) = attr_value(&event, &reader, b"val")? {
                        paragraph.apply_style(&style);
                    }
                }
                b"numPr" if in_paragraph => paragraph.is_list = true,
                b"ilvl" if in_paragraph => {
                    if let Some(level) = attr_value(&event, &reader, b"val")? {
                        paragraph.level = level.parse::<usize>().ok().map(|value| value + 1);
                    }
                }
                b"b" => run_style.bold = true,
                b"i" => run_style.italic = true,
                b"u" => run_style.underline = true,
                b"strike" => run_style.strikethrough = true,
                b"hyperlink" => {
                    let target = attr_value(&event, &reader, b"id")?
                        .and_then(|id| hyperlink_targets.get(&id).cloned());
                    hyperlink_stack.push(target);
                }
                b"tbl" => {
                    in_table = true;
                    table = TableState::default();
                }
                b"tr" if in_table => table.start_row(),
                b"tc" if in_table => table.start_cell(),
                b"t" if text_capture.kind != TextCaptureKind::Equation => {
                    text_capture = TextCapture::text()
                }
                b"tab" if in_paragraph => {
                    paragraph.push_text("\t", &run_style, hyperlink_stack.last())
                }
                b"br" | b"cr" if in_paragraph => {
                    paragraph.push_text("\n", &run_style, hyperlink_stack.last())
                }
                b"oMath" => text_capture = TextCapture::equation(),
                b"blip" | b"imagedata" => {
                    let mut image_context = DocxImageContext {
                        images: &mut images,
                        blocks: &mut blocks,
                        table: &mut table,
                        paragraph: &mut paragraph,
                        discarded_blocks: &mut discarded_blocks,
                        warnings: &mut warnings,
                    };
                    image_context.handle(
                        extract_docx_image(&package, &media_writer, &event, &reader).await?,
                        in_table,
                        in_paragraph,
                    );
                }
                _ => {}
            },
            Ok(Event::Empty(event)) => match local_name(event.name().as_ref()) {
                b"pStyle" if in_paragraph => {
                    if let Some(style) = attr_value(&event, &reader, b"val")? {
                        paragraph.apply_style(&style);
                    }
                }
                b"numPr" if in_paragraph => paragraph.is_list = true,
                b"ilvl" if in_paragraph => {
                    if let Some(level) = attr_value(&event, &reader, b"val")? {
                        paragraph.level = level.parse::<usize>().ok().map(|value| value + 1);
                    }
                }
                b"b" => run_style.bold = true,
                b"i" => run_style.italic = true,
                b"u" => run_style.underline = true,
                b"strike" => run_style.strikethrough = true,
                b"tab" if in_paragraph => {
                    paragraph.push_text("\t", &run_style, hyperlink_stack.last())
                }
                b"br" | b"cr" if in_paragraph => {
                    paragraph.push_text("\n", &run_style, hyperlink_stack.last())
                }
                b"blip" | b"imagedata" => {
                    let mut image_context = DocxImageContext {
                        images: &mut images,
                        blocks: &mut blocks,
                        table: &mut table,
                        paragraph: &mut paragraph,
                        discarded_blocks: &mut discarded_blocks,
                        warnings: &mut warnings,
                    };
                    image_context.handle(
                        extract_docx_image(&package, &media_writer, &event, &reader).await?,
                        in_table,
                        in_paragraph,
                    );
                }
                _ => {}
            },
            Ok(Event::Text(text)) => {
                let value = text
                    .decode()
                    .map_err(|error| ApiError::BadRequest(error.to_string()))?
                    .into_owned();
                match text_capture.kind {
                    TextCaptureKind::Text => {
                        if in_table {
                            table.push_cell_text(&value);
                        }
                        if in_paragraph {
                            paragraph.push_text(&value, &run_style, hyperlink_stack.last());
                        }
                    }
                    TextCaptureKind::Equation => {
                        if in_table {
                            table.push_cell_text(&format!("<eq>{value}</eq>"));
                        }
                        if in_paragraph {
                            paragraph.push_equation(&value);
                        }
                    }
                    TextCaptureKind::None => {}
                }
            }
            Ok(Event::End(event)) => match local_name(event.name().as_ref()) {
                b"t" if text_capture.kind != TextCaptureKind::Equation => {
                    text_capture = TextCapture::none()
                }
                b"oMath" => text_capture = TextCapture::none(),
                b"hyperlink" => {
                    hyperlink_stack.pop();
                }
                b"p" => {
                    if in_table {
                        table.push_cell_text(&paragraph.render_plain());
                    } else {
                        paragraph.flush_as_block(&mut blocks);
                    }
                    paragraph = ParagraphState::default();
                    in_paragraph = false;
                }
                b"r" => run_style = RunStyle::default(),
                b"tc" if in_table => table.finish_cell(),
                b"tr" if in_table => table.finish_row(),
                b"tbl" => {
                    if let Some(html) = table.finish_html() {
                        blocks.push(OfficeBlock::Table { html });
                    }
                    table = TableState::default();
                    in_table = false;
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(error) => return Err(ApiError::BadRequest(error.to_string())),
        }
    }

    let office_document = OfficeDocument {
        pages: vec![OfficePage {
            page_idx: 0,
            blocks: merge_adjacent_list_blocks(blocks),
            discarded_blocks,
        }],
        images,
        model_output: json!({
            "type": "docx",
            "source": upload.stem,
            "warnings": warnings
        }),
    };
    Ok(to_parsed_document(upload.stem.clone(), office_document))
}

#[derive(Debug, Clone, Default)]
struct ParagraphState {
    parts: Vec<String>,
    is_list: bool,
    level: Option<usize>,
}

impl ParagraphState {
    fn from_start(event: &BytesStart<'_>, reader: &quick_xml::Reader<&[u8]>) -> ApiResult<Self> {
        let mut state = Self::default();
        if let Some(style) = attr_value(event, reader, b"val")? {
            state.apply_style(&style);
        }
        Ok(state)
    }

    fn apply_style(&mut self, style: &str) {
        let normalized = style.to_ascii_lowercase();
        if let Some(level) = normalized
            .strip_prefix("heading")
            .and_then(|value| value.parse::<usize>().ok())
        {
            self.level = Some(level.clamp(1, 6));
        }
        if normalized.contains("list") {
            self.is_list = true;
        }
    }

    fn push_text(&mut self, value: &str, style: &RunStyle, hyperlink: Option<&Option<String>>) {
        if value.is_empty() {
            return;
        }
        if let Some(Some(url)) = hyperlink {
            self.parts.push(format!(
                r#"<hyperlink>{}<url>{}</url></hyperlink>"#,
                style.wrap_text_tag(html_escape(value)),
                html_escape(url)
            ));
            return;
        }
        let rendered = style.wrap(html_escape(value));
        self.parts.push(rendered);
    }

    fn push_equation(&mut self, value: &str) {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            self.parts
                .push(format!("<eq>{}</eq>", html_escape(trimmed)));
        }
    }

    fn render_plain(&self) -> String {
        self.parts.join("")
    }

    fn flush_as_block(&mut self, blocks: &mut Vec<OfficeBlock>) -> bool {
        let content = self.render_plain().trim().to_string();
        if content.is_empty() {
            return false;
        }
        if self.is_list {
            blocks.push(OfficeBlock::List {
                items: vec![content],
            });
        } else if let Some(level) = self.level {
            blocks.push(OfficeBlock::Title { content, level });
        } else if is_equation_only(&content) {
            blocks.push(OfficeBlock::Equation {
                latex: content
                    .trim_start_matches("<eq>")
                    .trim_end_matches("</eq>")
                    .to_string(),
            });
        } else {
            blocks.push(OfficeBlock::Text { content });
        }
        self.clear();
        true
    }

    fn clear(&mut self) {
        self.parts.clear();
    }
}

#[derive(Debug, Clone, Default)]
struct RunStyle {
    bold: bool,
    italic: bool,
    underline: bool,
    strikethrough: bool,
}

impl RunStyle {
    fn wrap(&self, content: String) -> String {
        let styles = self.styles();
        if styles.is_empty() {
            return content;
        }
        format!(r#"<text style="{}">{}</text>"#, styles.join(","), content)
    }

    fn wrap_text_tag(&self, content: String) -> String {
        let styles = self.styles();
        if styles.is_empty() {
            return format!("<text>{content}</text>");
        }
        format!(r#"<text style="{}">{}</text>"#, styles.join(","), content)
    }

    fn styles(&self) -> Vec<&'static str> {
        let mut styles = Vec::new();
        if self.bold {
            styles.push("bold");
        }
        if self.italic {
            styles.push("italic");
        }
        if self.underline {
            styles.push("underline");
        }
        if self.strikethrough {
            styles.push("strikethrough");
        }
        styles
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextCaptureKind {
    None,
    Text,
    Equation,
}

#[derive(Debug, Clone, Copy)]
struct TextCapture {
    kind: TextCaptureKind,
}

impl Default for TextCapture {
    fn default() -> Self {
        Self::none()
    }
}

impl TextCapture {
    fn none() -> Self {
        Self {
            kind: TextCaptureKind::None,
        }
    }

    fn text() -> Self {
        Self {
            kind: TextCaptureKind::Text,
        }
    }

    fn equation() -> Self {
        Self {
            kind: TextCaptureKind::Equation,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct TableState {
    rows: Vec<Vec<String>>,
    current_row: Vec<String>,
    current_cell: String,
    in_cell: bool,
}

impl TableState {
    fn start_row(&mut self) {
        self.current_row.clear();
    }

    fn finish_row(&mut self) {
        if !self.current_cell.trim().is_empty() {
            self.finish_cell();
        }
        if !self.current_row.is_empty() {
            self.rows.push(std::mem::take(&mut self.current_row));
        }
    }

    fn start_cell(&mut self) {
        self.current_cell.clear();
        self.in_cell = true;
    }

    fn push_cell_text(&mut self, value: &str) {
        if self.in_cell && !value.trim().is_empty() {
            if !self.current_cell.is_empty() {
                self.current_cell.push(' ');
            }
            self.current_cell.push_str(value.trim());
        }
    }

    fn finish_cell(&mut self) {
        if self.in_cell {
            self.current_row
                .push(std::mem::take(&mut self.current_cell));
        }
        self.in_cell = false;
    }

    fn finish_html(&mut self) -> Option<String> {
        if self.rows.is_empty() {
            return None;
        }
        let rows = self
            .rows
            .iter()
            .map(|row| {
                let cells = row
                    .iter()
                    .map(|cell| format!("<td>{}</td>", cell))
                    .collect::<String>();
                format!("<tr>{cells}</tr>")
            })
            .collect::<String>();
        Some(format!("<table>{rows}</table>"))
    }
}

async fn extract_docx_image(
    package: &OoxmlPackage,
    media_writer: &OfficeMediaWriter,
    event: &BytesStart<'_>,
    reader: &quick_xml::Reader<&[u8]>,
) -> ApiResult<DocxImageExtraction> {
    let rel_id = attr_value(event, reader, b"embed")?
        .or_else(|| attr_value(event, reader, b"id").ok().flatten());
    let Some(rel_id) = rel_id else {
        return Ok(DocxImageExtraction::None);
    };
    let Some(part) = relationship_target_part(package, WORD_DOCUMENT_PART, &rel_id)? else {
        tracing::warn!(rel_id, "DOCX image relationship target missing");
        return Ok(DocxImageExtraction::Skipped {
            reason: "missing_relationship_target".to_string(),
            detail: format!("DOCX image relationship target missing for {rel_id}"),
        });
    };
    let Some(bytes) = package.read(&part) else {
        tracing::warn!(part, "DOCX image part missing");
        return Ok(DocxImageExtraction::Skipped {
            reason: "missing_image_part".to_string(),
            detail: format!("DOCX image part missing: {part}"),
        });
    };
    let suggested_name = Path::new(&part)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("image");
    match media_writer.write_image(suggested_name, bytes).await {
        Ok(image) => Ok(DocxImageExtraction::Image(image)),
        Err(error) => {
            tracing::warn!(error = %error.detail(), "failed to write DOCX image");
            Ok(DocxImageExtraction::Skipped {
                reason: "image_write_failed".to_string(),
                detail: error.detail(),
            })
        }
    }
}

#[derive(Debug)]
enum DocxImageExtraction {
    Image(OfficeImage),
    Skipped { reason: String, detail: String },
    None,
}

struct DocxImageContext<'a> {
    images: &'a mut Vec<OfficeImage>,
    blocks: &'a mut Vec<OfficeBlock>,
    table: &'a mut TableState,
    paragraph: &'a mut ParagraphState,
    discarded_blocks: &'a mut Vec<OfficeDiscardedBlock>,
    warnings: &'a mut Vec<String>,
}

impl DocxImageContext<'_> {
    fn handle(&mut self, result: DocxImageExtraction, in_table: bool, in_paragraph: bool) {
        match result {
            DocxImageExtraction::Image(image) => {
                let relative_path = image.file_name.clone();
                self.images.push(image);
                if in_table {
                    self.table
                        .push_cell_text(&format!(r#"<img src="{relative_path}" />"#));
                } else if in_paragraph {
                    if !self.paragraph.flush_as_block(self.blocks) {
                        self.paragraph.clear();
                    }
                    self.blocks.push(OfficeBlock::Image {
                        path: relative_path,
                        alt: "image".to_string(),
                    });
                } else {
                    self.blocks.push(OfficeBlock::Image {
                        path: relative_path,
                        alt: "image".to_string(),
                    });
                }
            }
            DocxImageExtraction::Skipped { reason, detail } => {
                self.warnings.push(format!("{reason}: {detail}"));
                self.discarded_blocks.push(OfficeDiscardedBlock {
                    index: self.discarded_blocks.len(),
                    reason,
                    detail,
                });
            }
            DocxImageExtraction::None => {}
        }
    }
}

fn merge_adjacent_list_blocks(blocks: Vec<OfficeBlock>) -> Vec<OfficeBlock> {
    let mut merged = Vec::new();
    for block in blocks {
        match (merged.last_mut(), block) {
            (Some(OfficeBlock::List { items }), OfficeBlock::List { items: next_items }) => {
                items.extend(next_items);
            }
            (_, other) => merged.push(other),
        }
    }
    merged
}

fn attr_value(
    event: &BytesStart<'_>,
    reader: &quick_xml::Reader<&[u8]>,
    wanted_local_name: &[u8],
) -> ApiResult<Option<String>> {
    for attr in event.attributes().flatten() {
        if local_name(attr.key.as_ref()) == wanted_local_name {
            return attr
                .decoded_and_normalized_value(quick_xml::XmlVersion::Implicit1_0, reader.decoder())
                .map(|value| Some(value.into_owned()))
                .map_err(|error| ApiError::BadRequest(error.to_string()));
        }
    }
    Ok(None)
}

fn is_equation_only(content: &str) -> bool {
    content.starts_with("<eq>") && content.ends_with("</eq>")
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
