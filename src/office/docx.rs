use std::{collections::HashMap, path::Path};

use crate::{
    domain::models::{ParseTask, ParsedDocument, StoredUpload},
    error::{ApiError, ApiResult},
    office::{
        markdown::to_parsed_document,
        model::{OfficeBlock, OfficeDiscardedBlock, OfficeDocument, OfficeImage, OfficePage},
        package::{local_name, OoxmlPackage},
        rels::{read_relationships, relationship_target_part},
        writer_adapter::{OfficeImageWrite, OfficeMediaWriter},
    },
};
use quick_xml::events::{BytesStart, Event};
use serde_json::json;

const WORD_DOCUMENT_PART: &str = "word/document.xml";
const WORD_STYLES_PART: &str = "word/styles.xml";
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
    let style_map = DocxStyleMap::from_package(&package)?;
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
                    paragraph = ParagraphState::from_start(&event, &reader, &style_map)?;
                }
                b"r" => run_style = RunStyle::default(),
                b"pStyle" if in_paragraph => {
                    if let Some(style) = attr_value(&event, &reader, b"val")? {
                        paragraph.apply_style(&style, &style_map);
                    }
                }
                b"numPr" if in_paragraph => paragraph.is_list = true,
                b"ilvl" if in_paragraph => {
                    if let Some(level) = parse_zero_based_level(&event, &reader)? {
                        paragraph.list_level = Some(level);
                    }
                }
                b"outlineLvl" if in_paragraph => {
                    if let Some(level) = parse_zero_based_level(&event, &reader)? {
                        paragraph.level = Some(level);
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
                        paragraph.apply_style(&style, &style_map);
                    }
                }
                b"numPr" if in_paragraph => paragraph.is_list = true,
                b"ilvl" if in_paragraph => {
                    if let Some(level) = parse_zero_based_level(&event, &reader)? {
                        paragraph.list_level = Some(level);
                    }
                }
                b"outlineLvl" if in_paragraph => {
                    if let Some(level) = parse_zero_based_level(&event, &reader)? {
                        paragraph.level = Some(level);
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
    list_level: Option<usize>,
    level: Option<usize>,
}

impl ParagraphState {
    fn from_start(
        event: &BytesStart<'_>,
        reader: &quick_xml::Reader<&[u8]>,
        style_map: &DocxStyleMap,
    ) -> ApiResult<Self> {
        let mut state = Self::default();
        if let Some(style) = attr_value(event, reader, b"val")? {
            state.apply_style(&style, style_map);
        }
        Ok(state)
    }

    fn apply_style(&mut self, style: &str, style_map: &DocxStyleMap) {
        if let Some(style_info) = style_map.resolve(style) {
            if let Some(level) = style_info.level {
                self.level = Some(level);
            }
            if style_info.is_list {
                self.is_list = true;
            }
        }
        let normalized = style.to_ascii_lowercase();
        if normalized == "title" {
            self.level = Some(1);
        }
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
        if let Some(level) = self.level {
            blocks.push(OfficeBlock::Title { content, level });
        } else if self.is_list {
            blocks.push(OfficeBlock::List {
                items: vec![content],
            });
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
struct DocxStyleMap {
    styles: HashMap<String, DocxStyleInfo>,
}

#[derive(Debug, Clone, Default)]
struct DocxStyleInfo {
    name: Option<String>,
    based_on: Option<String>,
    level: Option<usize>,
    is_list: bool,
}

impl DocxStyleMap {
    fn from_package(package: &OoxmlPackage) -> ApiResult<Self> {
        let Some(xml_bytes) = package.read(WORD_STYLES_PART) else {
            return Ok(Self::default());
        };
        let xml = String::from_utf8(xml_bytes.to_vec()).map_err(|error| {
            ApiError::BadRequest(format!("Invalid UTF-8 in {WORD_STYLES_PART}: {error}"))
        })?;
        Self::parse(&xml)
    }

    fn parse(xml: &str) -> ApiResult<Self> {
        let mut reader = quick_xml::Reader::from_str(xml);
        reader.config_mut().trim_text(true);
        let mut styles = HashMap::new();
        let mut current_id: Option<String> = None;
        let mut current_info = DocxStyleInfo::default();
        let mut in_paragraph_style = false;
        let mut style_depth = 0_usize;

        loop {
            match reader.read_event() {
                Ok(Event::Start(event)) => match local_name(event.name().as_ref()) {
                    b"style" => {
                        style_depth = 1;
                        let style_type = attr_value(&event, &reader, b"type")?;
                        in_paragraph_style = style_type
                            .as_deref()
                            .is_none_or(|value| value.eq_ignore_ascii_case("paragraph"));
                        current_id = attr_value(&event, &reader, b"styleId")?;
                        current_info = DocxStyleInfo::default();
                    }
                    _ if style_depth > 0 => {
                        style_depth += 1;
                        Self::apply_style_child(
                            &mut current_info,
                            in_paragraph_style,
                            &event,
                            &reader,
                        )?;
                    }
                    _ => {}
                },
                Ok(Event::Empty(event)) if style_depth > 0 => {
                    Self::apply_style_child(
                        &mut current_info,
                        in_paragraph_style,
                        &event,
                        &reader,
                    )?;
                }
                Ok(Event::End(event)) => {
                    if local_name(event.name().as_ref()) == b"style" && style_depth == 1 {
                        if in_paragraph_style {
                            if let Some(style_id) = current_id.take() {
                                current_info.infer_from_names(&style_id);
                                styles.insert(style_id.to_ascii_lowercase(), current_info.clone());
                            }
                        }
                        current_info = DocxStyleInfo::default();
                        in_paragraph_style = false;
                        style_depth = 0;
                    } else if style_depth > 0 {
                        style_depth = style_depth.saturating_sub(1);
                    }
                }
                Ok(Event::Eof) => break,
                Ok(_) => {}
                Err(error) => return Err(ApiError::BadRequest(error.to_string())),
            }
        }

        Ok(Self { styles })
    }

    fn apply_style_child(
        info: &mut DocxStyleInfo,
        in_paragraph_style: bool,
        event: &BytesStart<'_>,
        reader: &quick_xml::Reader<&[u8]>,
    ) -> ApiResult<()> {
        if !in_paragraph_style {
            return Ok(());
        }
        match local_name(event.name().as_ref()) {
            b"name" => info.name = attr_value(event, reader, b"val")?,
            b"basedOn" => info.based_on = attr_value(event, reader, b"val")?,
            b"outlineLvl" => {
                if let Some(level) = parse_zero_based_level(event, reader)? {
                    info.level = Some(level);
                }
            }
            b"numPr" => info.is_list = true,
            _ => {}
        }
        Ok(())
    }

    fn resolve(&self, style_id: &str) -> Option<DocxStyleInfo> {
        let mut resolved = DocxStyleInfo::default();
        let mut current_id = Some(style_id.to_ascii_lowercase());
        let mut seen = Vec::new();
        while let Some(style_id) = current_id {
            if seen.iter().any(|seen_id| seen_id == &style_id) {
                break;
            }
            seen.push(style_id.clone());
            let Some(info) = self.styles.get(&style_id) else {
                break;
            };
            if resolved.level.is_none() {
                resolved.level = info.level;
            }
            resolved.is_list |= info.is_list;
            if resolved.name.is_none() {
                resolved.name = info.name.clone();
            }
            current_id = info
                .based_on
                .as_ref()
                .map(|value| value.to_ascii_lowercase());
        }
        if resolved.level.is_some() || resolved.is_list || resolved.name.is_some() {
            Some(resolved)
        } else {
            None
        }
    }
}

impl DocxStyleInfo {
    fn infer_from_names(&mut self, style_id: &str) {
        if self.level.is_none() {
            for candidate in [Some(style_id), self.name.as_deref()].into_iter().flatten() {
                let normalized = candidate.to_ascii_lowercase().replace(' ', "");
                if normalized == "title" {
                    self.level = Some(1);
                    break;
                }
                if let Some(level) = normalized
                    .strip_prefix("heading")
                    .and_then(|value| value.parse::<usize>().ok())
                {
                    self.level = Some(level.clamp(1, 6));
                    break;
                }
            }
        }
        if !self.is_list {
            self.is_list = self
                .name
                .as_deref()
                .is_some_and(|name| name.to_ascii_lowercase().contains("list"))
                || style_id.to_ascii_lowercase().contains("list");
        }
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
    match media_writer
        .write_image(suggested_name, package.content_type(&part), bytes)
        .await
    {
        Ok(OfficeImageWrite::Written { image, warning }) => {
            Ok(DocxImageExtraction::Image { image, warning })
        }
        Ok(OfficeImageWrite::Skipped { reason, detail }) => {
            tracing::warn!(reason, detail, "skipped DOCX image");
            Ok(DocxImageExtraction::Skipped { reason, detail })
        }
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
    Image {
        image: OfficeImage,
        warning: Option<String>,
    },
    Skipped {
        reason: String,
        detail: String,
    },
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
            DocxImageExtraction::Image { image, warning } => {
                if let Some(warning) = warning {
                    self.warnings.push(warning);
                }
                let relative_path = image.display_path.clone();
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

fn parse_zero_based_level(
    event: &BytesStart<'_>,
    reader: &quick_xml::Reader<&[u8]>,
) -> ApiResult<Option<usize>> {
    Ok(attr_value(event, reader, b"val")?
        .and_then(|level| level.parse::<usize>().ok())
        .map(|level| (level + 1).clamp(1, 6)))
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
