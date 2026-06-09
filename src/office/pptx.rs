use std::path::Path;

use quick_xml::events::{BytesStart, Event};
use serde_json::json;

use crate::{
    domain::models::{ParseTask, ParsedDocument, StoredUpload},
    error::{ApiError, ApiResult},
    office::{
        markdown::to_parsed_document,
        model::{OfficeBlock, OfficeDocument, OfficeImage, OfficePage},
        package::{local_name, OoxmlPackage},
        rels::{read_relationships, relationship_target_part},
        writer_adapter::OfficeMediaWriter,
    },
};

const PRESENTATION_PART: &str = "ppt/presentation.xml";
const TINY_DECORATION_SIZE_EMU: i64 = 91_440;
const FULL_SLIDE_IMAGE_PERCENT: i64 = 98;
const FULL_SLIDE_ORIGIN_TOLERANCE_PERCENT: i64 = 1;

pub async fn parse_pptx(task: &ParseTask, upload: &StoredUpload) -> ApiResult<ParsedDocument> {
    let package = OoxmlPackage::open(&upload.path)?;
    if !package.contains(PRESENTATION_PART) {
        return Err(ApiError::BadRequest(
            "Invalid PPTX package: missing ppt/presentation.xml".to_string(),
        ));
    }
    let media_writer = OfficeMediaWriter::new(task, &upload.stem).await?;
    let slide_parts = slide_parts_in_order(&package)?;
    let slide_size = slide_size(&package)?;
    let mut pages = Vec::new();
    let mut images = Vec::new();
    for (page_idx, slide_part) in slide_parts.iter().enumerate() {
        let (mut blocks, mut page_images) =
            parse_slide(&package, &media_writer, slide_part, slide_size).await?;
        images.append(&mut page_images);
        if let Some(notes_part) = notes_part_for_slide(&package, slide_part)? {
            let notes = parse_notes(&package, &notes_part)?;
            if !notes.trim().is_empty() {
                blocks.push(OfficeBlock::Text {
                    content: format!("Notes: {}", notes.trim()),
                });
            }
        }
        pages.push(OfficePage {
            page_idx,
            blocks,
            discarded_blocks: Vec::new(),
        });
    }

    let office_document = OfficeDocument {
        pages,
        images,
        model_output: json!({
            "type": "pptx",
            "source": upload.stem
        }),
    };
    Ok(to_parsed_document(upload.stem.clone(), office_document))
}

fn slide_parts_in_order(package: &OoxmlPackage) -> ApiResult<Vec<String>> {
    let xml = package.read_text(PRESENTATION_PART)?;
    let relationships = read_relationships(package, PRESENTATION_PART)?;
    let mut reader = quick_xml::Reader::from_str(&xml);
    reader.config_mut().trim_text(true);
    let mut slide_parts = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(event)) | Ok(Event::Empty(event)) => {
                if local_name(event.name().as_ref()) == b"sldId" {
                    if let Some(rel_id) = attr_value(&event, &reader, b"id")? {
                        if let Some(rel) = relationships.iter().find(|rel| rel.id == rel_id) {
                            if let Some(target) = rel.target_part(PRESENTATION_PART) {
                                slide_parts.push(target);
                            }
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(error) => return Err(ApiError::BadRequest(error.to_string())),
        }
    }
    if slide_parts.is_empty() {
        slide_parts.extend(
            package
                .part_names()
                .filter(|name| name.starts_with("ppt/slides/slide") && name.ends_with(".xml"))
                .map(ToOwned::to_owned)
                .collect::<Vec<String>>(),
        );
        slide_parts.sort();
    }
    Ok(slide_parts)
}

fn slide_size(package: &OoxmlPackage) -> ApiResult<Option<PptSlideSize>> {
    let xml = package.read_text(PRESENTATION_PART)?;
    let mut reader = quick_xml::Reader::from_str(&xml);
    reader.config_mut().trim_text(true);
    loop {
        match reader.read_event() {
            Ok(Event::Start(event)) | Ok(Event::Empty(event)) => {
                if local_name(event.name().as_ref()) == b"sldSz" {
                    let width = attr_i64(&event, &reader, b"cx")?;
                    let height = attr_i64(&event, &reader, b"cy")?;
                    if let (Some(width), Some(height)) = (width, height) {
                        return Ok(Some(PptSlideSize { width, height }));
                    }
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(error) => return Err(ApiError::BadRequest(error.to_string())),
        }
    }
    Ok(None)
}

async fn parse_slide(
    package: &OoxmlPackage,
    media_writer: &OfficeMediaWriter,
    slide_part: &str,
    slide_size: Option<PptSlideSize>,
) -> ApiResult<(Vec<OfficeBlock>, Vec<OfficeImage>)> {
    let xml = package.read_text(slide_part)?;
    let mut reader = quick_xml::Reader::from_str(&xml);
    reader.config_mut().trim_text(false);
    let mut items = Vec::new();
    let mut next_sequence = 0_usize;
    let mut current_element: Option<PptElementState> = None;
    let mut text_capture = false;
    let mut xfrm_depth = 0_usize;

    loop {
        match reader.read_event() {
            Ok(Event::Start(event)) => match local_name(event.name().as_ref()) {
                b"sp" if current_element.is_none() => {
                    current_element = Some(PptElementState::new(PptElementKind::Shape));
                }
                b"pic" if current_element.is_none() => {
                    current_element = Some(PptElementState::new(PptElementKind::Picture));
                }
                b"graphicFrame" if current_element.is_none() => {
                    current_element = Some(PptElementState::new(PptElementKind::GraphicFrame));
                }
                b"xfrm" if current_element.is_some() => xfrm_depth += 1,
                b"off" | b"ext" if xfrm_depth > 0 => {
                    if let Some(element) = current_element.as_mut() {
                        element.update_transform(&event, &reader)?;
                    }
                }
                b"t" => text_capture = true,
                b"tbl" => {
                    if let Some(element) = current_element.as_mut() {
                        element.start_table();
                    }
                }
                b"tr" => {
                    if let Some(element) = current_element.as_mut() {
                        element.start_table_row();
                    }
                }
                b"tc" => {
                    if let Some(element) = current_element.as_mut() {
                        element.start_table_cell();
                    }
                }
                b"blip" => {
                    if let Some(element) = current_element.as_mut() {
                        element.set_image_rel_id(&event, &reader)?;
                    }
                }
                b"chart" => {
                    if let Some(element) = current_element.as_mut() {
                        element.mark_chart();
                    }
                }
                _ => {}
            },
            Ok(Event::Empty(event)) => match local_name(event.name().as_ref()) {
                b"off" | b"ext" if xfrm_depth > 0 => {
                    if let Some(element) = current_element.as_mut() {
                        element.update_transform(&event, &reader)?;
                    }
                }
                b"blip" => {
                    if let Some(element) = current_element.as_mut() {
                        element.set_image_rel_id(&event, &reader)?;
                    }
                }
                b"chart" => {
                    if let Some(element) = current_element.as_mut() {
                        element.mark_chart();
                    }
                }
                _ => {}
            },
            Ok(Event::Text(text)) => {
                let value = text
                    .decode()
                    .map_err(|error| ApiError::BadRequest(error.to_string()))?
                    .into_owned();
                if text_capture {
                    if let Some(element) = current_element.as_mut() {
                        element.push_text(&value);
                    }
                }
            }
            Ok(Event::End(event)) => match local_name(event.name().as_ref()) {
                b"t" => text_capture = false,
                b"p" => {
                    if let Some(element) = current_element.as_mut() {
                        element.finish_paragraph();
                    }
                }
                b"tc" => {
                    if let Some(element) = current_element.as_mut() {
                        element.finish_table_cell();
                    }
                }
                b"tr" => {
                    if let Some(element) = current_element.as_mut() {
                        element.finish_table_row();
                    }
                }
                b"tbl" => {
                    if let Some(element) = current_element.as_mut() {
                        element.finish_table();
                    }
                }
                b"xfrm" if xfrm_depth > 0 => xfrm_depth -= 1,
                b"sp" => {
                    if let Some(element) = current_element.take() {
                        if element.kind == PptElementKind::Shape {
                            push_slide_item(
                                &mut items,
                                &mut next_sequence,
                                element
                                    .finish(package, media_writer, slide_part, slide_size)
                                    .await?,
                            );
                        } else {
                            current_element = Some(element);
                        }
                    }
                }
                b"pic" => {
                    if let Some(element) = current_element.take() {
                        if element.kind == PptElementKind::Picture {
                            push_slide_item(
                                &mut items,
                                &mut next_sequence,
                                element
                                    .finish(package, media_writer, slide_part, slide_size)
                                    .await?,
                            );
                        } else {
                            current_element = Some(element);
                        }
                    }
                }
                b"graphicFrame" => {
                    if let Some(element) = current_element.take() {
                        if element.kind == PptElementKind::GraphicFrame {
                            push_slide_item(
                                &mut items,
                                &mut next_sequence,
                                element
                                    .finish(package, media_writer, slide_part, slide_size)
                                    .await?,
                            );
                        } else {
                            current_element = Some(element);
                        }
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(error) => return Err(ApiError::BadRequest(error.to_string())),
        }
    }

    if let Some(element) = current_element.take() {
        push_slide_item(
            &mut items,
            &mut next_sequence,
            element
                .finish(package, media_writer, slide_part, slide_size)
                .await?,
        );
    }

    items.sort_by_key(PptSlideItem::sort_key);
    let mut blocks = Vec::new();
    let mut images = Vec::new();
    for item in items {
        match item.block {
            PptItemBlock::Text(content) => push_text_block(&mut blocks, content),
            PptItemBlock::Table(html) => blocks.push(OfficeBlock::Table { html }),
            PptItemBlock::Image(image) => {
                let path = image.file_name.clone();
                images.push(image);
                blocks.push(OfficeBlock::Image {
                    path,
                    alt: "image".to_string(),
                });
            }
            PptItemBlock::Chart(html) => blocks.push(OfficeBlock::Chart { html }),
        }
    }
    Ok((blocks, images))
}

fn parse_notes(package: &OoxmlPackage, notes_part: &str) -> ApiResult<String> {
    let xml = package.read_text(notes_part)?;
    let mut reader = quick_xml::Reader::from_str(&xml);
    reader.config_mut().trim_text(false);
    let mut capture = false;
    let mut notes = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(event)) => {
                if local_name(event.name().as_ref()) == b"t" {
                    capture = true;
                }
            }
            Ok(Event::Text(text)) if capture => {
                let value = text
                    .decode()
                    .map_err(|error| ApiError::BadRequest(error.to_string()))?
                    .into_owned();
                if !value.trim().is_empty() {
                    notes.push(value.trim().to_string());
                }
            }
            Ok(Event::End(event)) => {
                if local_name(event.name().as_ref()) == b"t" {
                    capture = false;
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(error) => return Err(ApiError::BadRequest(error.to_string())),
        }
    }
    Ok(notes.join(" "))
}

fn notes_part_for_slide(package: &OoxmlPackage, slide_part: &str) -> ApiResult<Option<String>> {
    Ok(read_relationships(package, slide_part)?
        .into_iter()
        .find(|rel| rel.rel_type.ends_with("/notesSlide"))
        .and_then(|rel| rel.target_part(slide_part)))
}

#[derive(Debug, Clone, Copy)]
struct PptSlideSize {
    width: i64,
    height: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PptElementKind {
    Shape,
    Picture,
    GraphicFrame,
}

#[derive(Debug, Clone, Copy, Default)]
struct PptTransform {
    x: Option<i64>,
    y: Option<i64>,
    width: Option<i64>,
    height: Option<i64>,
}

impl PptTransform {
    /// Update slide item geometry from OOXML transform children.
    ///
    /// Inputs:
    /// - `event`: `<off>` or `<ext>` transform element.
    /// - `reader`: XML reader used to decode attributes.
    fn update_from_event(
        &mut self,
        event: &BytesStart<'_>,
        reader: &quick_xml::Reader<&[u8]>,
    ) -> ApiResult<()> {
        match local_name(event.name().as_ref()) {
            b"off" => {
                self.x = attr_i64(event, reader, b"x")?;
                self.y = attr_i64(event, reader, b"y")?;
            }
            b"ext" => {
                self.width = attr_i64(event, reader, b"cx")?;
                self.height = attr_i64(event, reader, b"cy")?;
            }
            _ => {}
        }
        Ok(())
    }

    fn sort_position(self) -> (Option<i64>, Option<i64>) {
        (self.y, self.x)
    }

    fn is_tiny_decoration(self) -> bool {
        match (self.width, self.height) {
            (Some(width), Some(height)) => {
                width <= TINY_DECORATION_SIZE_EMU && height <= TINY_DECORATION_SIZE_EMU
            }
            _ => false,
        }
    }

    fn is_full_slide_background(self, slide_size: Option<PptSlideSize>) -> bool {
        let Some(slide_size) = slide_size else {
            return false;
        };
        let (Some(x), Some(y), Some(width), Some(height)) =
            (self.x, self.y, self.width, self.height)
        else {
            return false;
        };
        let near_left = within_percent_of_origin(x, slide_size.width);
        let near_top = within_percent_of_origin(y, slide_size.height);
        let covers_width = covers_percent(width, slide_size.width, FULL_SLIDE_IMAGE_PERCENT);
        let covers_height = covers_percent(height, slide_size.height, FULL_SLIDE_IMAGE_PERCENT);
        near_left && near_top && covers_width && covers_height
    }
}

#[derive(Debug, Clone)]
struct PptElementState {
    kind: PptElementKind,
    transform: PptTransform,
    text: String,
    table: Option<PptTableState>,
    table_html: Option<String>,
    chart_text: Vec<String>,
    has_chart: bool,
    image_rel_id: Option<String>,
}

impl PptElementState {
    fn new(kind: PptElementKind) -> Self {
        Self {
            kind,
            transform: PptTransform::default(),
            text: String::new(),
            table: None,
            table_html: None,
            chart_text: Vec::new(),
            has_chart: false,
            image_rel_id: None,
        }
    }

    fn update_transform(
        &mut self,
        event: &BytesStart<'_>,
        reader: &quick_xml::Reader<&[u8]>,
    ) -> ApiResult<()> {
        self.transform.update_from_event(event, reader)
    }

    fn set_image_rel_id(
        &mut self,
        event: &BytesStart<'_>,
        reader: &quick_xml::Reader<&[u8]>,
    ) -> ApiResult<()> {
        self.image_rel_id = attr_value(event, reader, b"embed")?;
        Ok(())
    }

    fn mark_chart(&mut self) {
        self.has_chart = true;
    }

    fn start_table(&mut self) {
        self.table = Some(PptTableState::default());
    }

    fn start_table_row(&mut self) {
        if let Some(table) = self.table.as_mut() {
            table.start_row();
        }
    }

    fn start_table_cell(&mut self) {
        if let Some(table) = self.table.as_mut() {
            table.start_cell();
        }
    }

    fn finish_table_cell(&mut self) {
        if let Some(table) = self.table.as_mut() {
            table.finish_cell();
        }
    }

    fn finish_table_row(&mut self) {
        if let Some(table) = self.table.as_mut() {
            table.finish_row();
        }
    }

    fn finish_table(&mut self) {
        if let Some(mut table) = self.table.take() {
            self.table_html = table.finish_html();
        }
    }

    fn push_text(&mut self, value: &str) {
        if let Some(table) = self.table.as_mut() {
            table.push_cell_text(value);
        } else if self.kind == PptElementKind::GraphicFrame || self.has_chart {
            if !value.trim().is_empty() {
                self.chart_text.push(value.trim().to_string());
            }
        } else {
            if !self.text.is_empty() && !self.text.ends_with('\n') {
                self.text.push(' ');
            }
            self.text.push_str(value.trim());
        }
    }

    fn finish_paragraph(&mut self) {
        if self.table.is_none() && !self.text.trim().is_empty() {
            self.text.push('\n');
        }
    }

    /// Convert the collected element into one positioned slide item.
    ///
    /// Inputs:
    /// - `package`: PPTX package used to read image relationships.
    /// - `media_writer`: output writer for retained images.
    /// - `slide_part`: current slide XML part.
    /// - `slide_size`: presentation canvas size for background filtering.
    async fn finish(
        mut self,
        package: &OoxmlPackage,
        media_writer: &OfficeMediaWriter,
        slide_part: &str,
        slide_size: Option<PptSlideSize>,
    ) -> ApiResult<Option<PptElementOutput>> {
        let transform = self.transform;
        if self.table.is_some() {
            self.finish_table();
        }

        if self.kind == PptElementKind::Picture {
            if self.transform.is_tiny_decoration()
                || self.transform.is_full_slide_background(slide_size)
            {
                return Ok(None);
            }
            let Some(rel_id) = self.image_rel_id else {
                return Ok(None);
            };
            return extract_pptx_image(package, media_writer, slide_part, &rel_id)
                .await
                .map(|image| {
                    image.map(|image| PptElementOutput {
                        transform,
                        block: PptItemBlock::Image(image),
                    })
                });
        }

        if let Some(html) = self.table_html {
            return Ok(Some(PptElementOutput {
                transform,
                block: PptItemBlock::Table(html),
            }));
        }

        let chart_values = self
            .chart_text
            .into_iter()
            .filter(|value| !value.trim().is_empty())
            .collect::<Vec<String>>();
        if !chart_values.is_empty() {
            return Ok(Some(PptElementOutput {
                transform,
                block: PptItemBlock::Chart(chart_text_to_table(&chart_values)),
            }));
        }

        let content = self.text.trim().to_string();
        if content.is_empty() {
            return Ok(None);
        }
        Ok(Some(PptElementOutput {
            transform,
            block: PptItemBlock::Text(content),
        }))
    }
}

#[derive(Debug, Clone)]
struct PptElementOutput {
    transform: PptTransform,
    block: PptItemBlock,
}

#[derive(Debug, Clone)]
struct PptSlideItem {
    sequence: usize,
    y: Option<i64>,
    x: Option<i64>,
    block: PptItemBlock,
}

impl PptSlideItem {
    fn sort_key(&self) -> (i32, i64, i64, usize) {
        match (self.y, self.x) {
            (Some(y), Some(x)) => (0, y, x, self.sequence),
            (Some(y), None) => (0, y, i64::MAX, self.sequence),
            _ => (1, i64::MAX, i64::MAX, self.sequence),
        }
    }
}

#[derive(Debug, Clone)]
enum PptItemBlock {
    Text(String),
    Table(String),
    Image(OfficeImage),
    Chart(String),
}

fn push_slide_item(
    items: &mut Vec<PptSlideItem>,
    next_sequence: &mut usize,
    item: Option<PptElementOutput>,
) {
    let Some(output) = item else {
        return;
    };
    let (y, x) = output.transform.sort_position();
    items.push(PptSlideItem {
        sequence: *next_sequence,
        y,
        x,
        block: output.block,
    });
    *next_sequence += 1;
}

async fn extract_pptx_image(
    package: &OoxmlPackage,
    media_writer: &OfficeMediaWriter,
    slide_part: &str,
    rel_id: &str,
) -> ApiResult<Option<OfficeImage>> {
    let Some(part) = relationship_target_part(package, slide_part, rel_id)? else {
        tracing::warn!(rel_id, slide_part, "PPTX image relationship target missing");
        return Ok(None);
    };
    let Some(bytes) = package.read(&part) else {
        tracing::warn!(part, "PPTX image part missing");
        return Ok(None);
    };
    let suggested_name = Path::new(&part)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("image");
    match media_writer.write_image(suggested_name, bytes).await {
        Ok(image) => Ok(Some(image)),
        Err(error) => {
            tracing::warn!(error = %error.detail(), "failed to write PPTX image");
            Ok(None)
        }
    }
}

fn push_text_block(blocks: &mut Vec<OfficeBlock>, content: String) {
    if !content.is_empty() {
        if content.len() < 80 && blocks.is_empty() {
            blocks.push(OfficeBlock::Title { content, level: 1 });
        } else {
            blocks.push(OfficeBlock::Text { content });
        }
    }
}

fn chart_text_to_table(values: &[String]) -> String {
    let rows = values
        .iter()
        .filter(|value| !value.trim().is_empty())
        .map(|value| format!("<tr><td>{}</td></tr>", html_escape(value.trim())))
        .collect::<String>();
    format!("<table>{rows}</table>")
}

#[derive(Debug, Clone, Default)]
struct PptTableState {
    rows: Vec<Vec<String>>,
    current_row: Vec<String>,
    current_cell: String,
    in_cell: bool,
}

impl PptTableState {
    fn start_row(&mut self) {
        self.current_row.clear();
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
            self.current_cell.push_str(&html_escape(value.trim()));
        }
    }

    fn finish_cell(&mut self) {
        if self.in_cell {
            self.current_row
                .push(std::mem::take(&mut self.current_cell));
        }
        self.in_cell = false;
    }

    fn finish_row(&mut self) {
        if !self.current_cell.is_empty() {
            self.finish_cell();
        }
        if !self.current_row.is_empty() {
            self.rows.push(std::mem::take(&mut self.current_row));
        }
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
                    .map(|cell| format!("<td>{cell}</td>"))
                    .collect::<String>();
                format!("<tr>{cells}</tr>")
            })
            .collect::<String>();
        Some(format!("<table>{rows}</table>"))
    }
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

fn attr_i64(
    event: &BytesStart<'_>,
    reader: &quick_xml::Reader<&[u8]>,
    wanted_local_name: &[u8],
) -> ApiResult<Option<i64>> {
    let Some(value) = attr_value(event, reader, wanted_local_name)? else {
        return Ok(None);
    };
    value
        .parse::<i64>()
        .map(Some)
        .map_err(|error| ApiError::BadRequest(error.to_string()))
}

fn within_percent_of_origin(value: i64, reference: i64) -> bool {
    if reference <= 0 {
        return false;
    }
    value.abs().saturating_mul(100) <= reference.saturating_mul(FULL_SLIDE_ORIGIN_TOLERANCE_PERCENT)
}

fn covers_percent(value: i64, reference: i64, percent: i64) -> bool {
    if reference <= 0 {
        return false;
    }
    value.saturating_mul(100) >= reference.saturating_mul(percent)
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
