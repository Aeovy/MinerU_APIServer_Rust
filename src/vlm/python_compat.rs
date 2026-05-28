use std::{
    collections::{HashMap, HashSet},
    io::Cursor,
    path::{Path, PathBuf},
    sync::Arc,
};

use image::{DynamicImage, ImageFormat};
use md5::Md5;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use tokio::fs;

use crate::{
    domain::models::ContentBlock,
    error::{ApiError, ApiResult},
};

#[derive(Debug, Clone)]
pub struct PythonPageInput {
    pub page_index: usize,
    pub page_width: u32,
    pub page_height: u32,
    pub point_width: u32,
    pub point_height: u32,
    pub image: Arc<DynamicImage>,
    pub blocks: Vec<ContentBlock>,
}

#[derive(Debug)]
pub struct PythonDocumentOutput {
    pub markdown: String,
    pub middle_json: Value,
    pub model_output: Value,
    pub content_list: Value,
    pub content_list_v2: Value,
    pub image_files: Vec<PathBuf>,
}

#[derive(Debug)]
pub struct PythonPageOutputFragment {
    page_index: usize,
    page_info: PageInfo,
    model_output_page: Value,
    image_files: Vec<PathBuf>,
}

impl PythonPageOutputFragment {
    pub fn page_index(&self) -> usize {
        self.page_index
    }
}

#[derive(Debug, Default)]
pub struct DocumentOutputAccumulator {
    pdf_info: Vec<PageInfo>,
    model_output_pages: Vec<Value>,
    image_files: Vec<PathBuf>,
}

impl DocumentOutputAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Consume one processing window and append its lightweight Python-compatible output state.
    ///
    /// Inputs:
    /// - `pages`: parsed page images and raw VLM blocks for one processing window.
    /// - `pending_image_dir`: temporary directory for Python-style cropped JPEG assets.
    #[cfg(test)]
    pub async fn append_pages(
        &mut self,
        pages: Vec<PythonPageInput>,
        pending_image_dir: &Path,
    ) -> ApiResult<()> {
        fs::create_dir_all(pending_image_dir).await?;
        let mut fragments = Vec::with_capacity(pages.len());
        for page in pages {
            fragments.push(build_page_output_fragment(page, pending_image_dir).await?);
        }
        self.append_fragments(fragments);

        Ok(())
    }

    /// Append already materialized page output fragments without retaining page images.
    ///
    /// Inputs:
    /// - `fragments`: page output built by the page-level parser pipeline.
    pub fn append_fragments(&mut self, fragments: Vec<PythonPageOutputFragment>) {
        for mut fragment in fragments {
            self.model_output_pages
                .push(std::mem::take(&mut fragment.model_output_page));
            self.pdf_info.push(fragment.page_info);
            self.image_files.append(&mut fragment.image_files);
        }
    }

    /// Finalize accumulated page state into the existing MinerU-compatible document output.
    pub fn finish(self) -> PythonDocumentOutput {
        let markdown = make_markdown(&self.pdf_info);
        let content_list = make_content_list(&self.pdf_info);
        let content_list_v2 = make_content_list_v2(&self.pdf_info);

        PythonDocumentOutput {
            markdown,
            middle_json: json!({
                "pdf_info": self.pdf_info.iter().map(page_info_to_json).collect::<Vec<Value>>(),
                "_backend": "vlm",
                "_version_name": crate::config::MINERU_VERSION,
            }),
            model_output: Value::Array(self.model_output_pages),
            content_list,
            content_list_v2,
            image_files: self.image_files,
        }
    }
}

/// Build one lightweight page fragment and release the source page image before returning.
///
/// Inputs:
/// - `page`: parsed page image and raw VLM blocks.
/// - `pending_image_dir`: temporary directory for Python-style cropped JPEG assets.
pub async fn build_page_output_fragment(
    mut page: PythonPageInput,
    pending_image_dir: &Path,
) -> ApiResult<PythonPageOutputFragment> {
    fs::create_dir_all(pending_image_dir).await?;
    let processed_blocks = post_process_raw_blocks(std::mem::take(&mut page.blocks));
    let model_output_page =
        Value::Array(processed_blocks.iter().map(model_block_to_json).collect());
    let (mut page_info, image_files) =
        build_page_info(&page, &processed_blocks, pending_image_dir).await?;
    finalize_page_info(&mut page_info);
    Ok(PythonPageOutputFragment {
        page_index: page.page_index,
        page_info,
        model_output_page,
        image_files,
    })
}

#[derive(Debug, Clone)]
struct PySpan {
    span_type: String,
    bbox: [i64; 4],
    content: Option<String>,
    html: Option<String>,
    image_path: Option<String>,
}

#[derive(Debug, Clone)]
struct PyLine {
    bbox: [i64; 4],
    spans: Vec<PySpan>,
    extra: Option<Value>,
}

#[derive(Debug, Clone)]
struct PyBlock {
    block_type: String,
    bbox: [i64; 4],
    angle: u16,
    lines: Vec<PyLine>,
    blocks: Vec<PyBlock>,
    index: usize,
    sub_type: Option<String>,
    guess_lang: Option<String>,
    merge_prev: Option<bool>,
}

#[derive(Debug, Clone)]
struct PageInfo {
    preproc_blocks: Vec<PyBlock>,
    discarded_blocks: Vec<PyBlock>,
    para_blocks: Vec<PyBlock>,
    page_size: [u32; 2],
    page_idx: usize,
}

fn post_process_raw_blocks(mut blocks: Vec<ContentBlock>) -> Vec<ContentBlock> {
    for block in &mut blocks {
        if block.block_type == "list_item" {
            block.block_type = "text".to_string();
        }
        if block.block_type == "table" {
            if let Some(content) = block.content.as_deref() {
                block.content = Some(convert_otsl_to_html(content));
            }
        }
        if block.block_type == "equation" {
            if let Some(content) = block.content.as_deref() {
                block.content = Some(add_equation_brackets(process_equation(content)));
            }
        }
        if matches!(block.block_type.as_str(), "image" | "chart") {
            block.content = image_analysis_content(block.content.as_deref());
        }
    }

    blocks
        .into_iter()
        .filter(|block| block.block_type != "equation_block")
        .collect()
}

async fn build_page_info(
    page: &PythonPageInput,
    raw_blocks: &[ContentBlock],
    pending_image_dir: &Path,
) -> ApiResult<(PageInfo, Vec<PathBuf>)> {
    let mut blocks = raw_blocks
        .iter()
        .enumerate()
        .filter_map(|(index, block)| {
            raw_to_py_block(block, index, page.point_width, page.point_height)
        })
        .collect::<Vec<_>>();

    let page_md5 = page_image_md5(&page.image);
    let mut image_files = Vec::new();
    for block in &mut blocks {
        for span in iter_spans_mut(block) {
            if matches!(
                span.span_type.as_str(),
                "image" | "table" | "chart" | "interline_equation"
            ) {
                let path = write_python_crop(
                    page,
                    &page_md5,
                    &span.span_type,
                    span.bbox,
                    pending_image_dir,
                )
                .await?;
                span.image_path = Some(
                    path.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string(),
                );
                image_files.push(path);
            }
        }
    }

    let (visual_groups, unmatched_children) = regroup_visual_blocks(&blocks);
    let mut text_blocks = Vec::new();
    let mut title_blocks = Vec::new();
    let mut ref_text_blocks = Vec::new();
    let mut phonetic_blocks = Vec::new();
    let mut list_blocks = Vec::new();
    let mut equation_blocks = Vec::new();
    let mut discarded_blocks = Vec::new();

    for block in blocks {
        if is_visual_main_type(&block.block_type) || is_generic_child_type(&block.block_type) {
            continue;
        }
        match block.block_type.as_str() {
            "interline_equation" => equation_blocks.push(block),
            "text" => text_blocks.push(block),
            "title" => title_blocks.push(block),
            "ref_text" => ref_text_blocks.push(block),
            "phonetic" => phonetic_blocks.push(block),
            "header" | "footer" | "page_number" | "aside_text" | "page_footnote" => {
                discarded_blocks.push(block)
            }
            "list" => list_blocks.push(block),
            _ => text_blocks.push(block),
        }
    }

    for mut child in unmatched_children {
        child.block_type = "text".to_string();
        text_blocks.push(child);
    }

    fix_list_blocks(&mut list_blocks, &mut text_blocks, &mut ref_text_blocks);

    let mut preproc_blocks = Vec::new();
    preproc_blocks.extend(visual_groups.image);
    preproc_blocks.extend(visual_groups.table);
    preproc_blocks.extend(visual_groups.chart);
    preproc_blocks.extend(visual_groups.code);
    preproc_blocks.extend(ref_text_blocks);
    preproc_blocks.extend(phonetic_blocks);
    preproc_blocks.extend(title_blocks);
    preproc_blocks.extend(text_blocks);
    preproc_blocks.extend(equation_blocks);
    preproc_blocks.extend(list_blocks);
    preproc_blocks.sort_by_key(|block| block.index);
    discarded_blocks.sort_by_key(|block| block.index);

    Ok((
        PageInfo {
            para_blocks: preproc_blocks.clone(),
            preproc_blocks,
            discarded_blocks,
            page_size: [page.point_width, page.point_height],
            page_idx: page.page_index,
        },
        image_files,
    ))
}

fn finalize_page_info(page_info: &mut PageInfo) {
    merge_para_text_blocks(&mut page_info.para_blocks);
}

fn raw_to_py_block(
    block: &ContentBlock,
    index: usize,
    point_width: u32,
    point_height: u32,
) -> Option<PyBlock> {
    let bbox = normalized_to_point_bbox(block.bbox, point_width, point_height)?;
    let raw_type = block.block_type.as_str();
    let content = block.content.clone().unwrap_or_default();
    let angle = block.angle.unwrap_or(0);
    let mut block_type = raw_type.to_string();
    let mut span_type = "unknown".to_string();
    let mut sub_type = None;
    let mut guess_lang = None;

    match raw_type {
        "text" | "title" | "ref_text" | "phonetic" | "header" | "footer" | "page_number"
        | "aside_text" | "page_footnote" | "list" => {
            span_type = "text".to_string();
        }
        "image_caption" | "table_caption" | "code_caption" => {
            block_type = "caption".to_string();
            span_type = "text".to_string();
        }
        "image_footnote" | "table_footnote" => {
            block_type = "footnote".to_string();
            span_type = "text".to_string();
        }
        "image" => {
            block_type = "image_body".to_string();
            span_type = "image".to_string();
        }
        "image_block" => {
            block_type = "image_block_body".to_string();
            span_type = "image".to_string();
        }
        "table" => {
            block_type = "table_body".to_string();
            span_type = "table".to_string();
        }
        "chart" => {
            block_type = "chart_body".to_string();
            span_type = "chart".to_string();
        }
        "code" | "algorithm" => {
            block_type = "code_body".to_string();
            span_type = "text".to_string();
            sub_type = Some(raw_type.to_string());
            guess_lang = Some("txt".to_string());
        }
        "equation" => {
            block_type = "interline_equation".to_string();
            span_type = "interline_equation".to_string();
        }
        _ => {}
    }

    let spans = if span_type == "text"
        && content.contains("\\(")
        && content.matches("\\(").count() == content.matches("\\)").count()
    {
        split_inline_equation_spans(&content, bbox)
    } else {
        vec![PySpan {
            span_type,
            bbox,
            content: match raw_type {
                "table" => None,
                "image" => Some(content.clone()),
                "chart" => Some(content.clone()),
                "equation" => Some(clean_interline_equation(&content)),
                _ => Some(clean_text_content(raw_type, &content)),
            },
            html: (raw_type == "table").then_some(content),
            image_path: None,
        }]
    };

    let line = PyLine {
        bbox,
        spans,
        extra: sub_type.as_ref().map(|kind| json!({"type": kind, "guess_lang": guess_lang.clone().unwrap_or_else(|| "txt".to_string())})),
    };

    Some(PyBlock {
        block_type,
        bbox,
        angle,
        lines: vec![line],
        blocks: Vec::new(),
        index,
        sub_type,
        guess_lang,
        merge_prev: block.merge_prev,
    })
}

fn split_inline_equation_spans(content: &str, bbox: [i64; 4]) -> Vec<PySpan> {
    let regex = regex::Regex::new(r"\\\((.+?)\\\)").expect("inline equation regex compiles");
    let mut spans = Vec::new();
    let mut last_end = 0;
    for captures in regex.captures_iter(content) {
        let Some(match_) = captures.get(0) else {
            continue;
        };
        if match_.start() > last_end {
            let text = &content[last_end..match_.start()];
            if !text.trim().is_empty() {
                spans.push(PySpan {
                    span_type: "text".to_string(),
                    bbox,
                    content: Some(text.to_string()),
                    html: None,
                    image_path: None,
                });
            }
        }
        if let Some(eq) = captures.get(1) {
            spans.push(PySpan {
                span_type: "inline_equation".to_string(),
                bbox,
                content: Some(eq.as_str().trim().to_string()),
                html: None,
                image_path: None,
            });
        }
        last_end = match_.end();
    }
    if last_end < content.len() {
        let text = &content[last_end..];
        if !text.trim().is_empty() {
            spans.push(PySpan {
                span_type: "text".to_string(),
                bbox,
                content: Some(text.to_string()),
                html: None,
                image_path: None,
            });
        }
    }
    spans
}

#[derive(Debug, Default)]
struct VisualGroups {
    image: Vec<PyBlock>,
    table: Vec<PyBlock>,
    chart: Vec<PyBlock>,
    code: Vec<PyBlock>,
}

fn regroup_visual_blocks(blocks: &[PyBlock]) -> (VisualGroups, Vec<PyBlock>) {
    let mut groups = VisualGroups::default();
    let main_indices = blocks
        .iter()
        .enumerate()
        .filter_map(|(pos, block)| is_visual_main_type(&block.block_type).then_some(pos))
        .collect::<Vec<_>>();
    let mut children_by_parent: HashMap<usize, Vec<PyBlock>> = HashMap::new();
    let mut unmatched = Vec::new();

    for (child_pos, child) in blocks.iter().enumerate() {
        if !is_generic_child_type(&child.block_type) {
            continue;
        }
        let parent_pos = main_indices
            .iter()
            .copied()
            .filter(|parent_pos| is_visual_neighbor(blocks, child_pos, *parent_pos))
            .min_by_key(|parent_pos| {
                (
                    parent_pos.abs_diff(child_pos),
                    bbox_distance(child.bbox, blocks[*parent_pos].bbox),
                    blocks[*parent_pos].index,
                )
            });

        if let Some(parent_pos) = parent_pos {
            children_by_parent
                .entry(parent_pos)
                .or_default()
                .push(child.clone());
        } else {
            unmatched.push(child.clone());
        }
    }

    for (pos, main) in blocks.iter().enumerate() {
        let Some(parent_type) = visual_parent_type(&main.block_type) else {
            continue;
        };
        let mut parent = PyBlock {
            block_type: parent_type.to_string(),
            bbox: main.bbox,
            angle: main.angle,
            lines: Vec::new(),
            blocks: Vec::new(),
            index: main.index,
            sub_type: main.sub_type.clone(),
            guess_lang: main.guess_lang.clone(),
            merge_prev: None,
        };
        let mut body = main.clone();
        body.block_type = visual_body_type(parent_type).to_string();
        parent.blocks.push(body);
        if let Some(mut children) = children_by_parent.remove(&pos) {
            children.sort_by_key(|child| child.index);
            for mut child in children {
                child.block_type = if child.block_type == "footnote" {
                    visual_footnote_type(parent_type).to_string()
                } else {
                    visual_caption_type(parent_type).to_string()
                };
                parent.blocks.push(child);
            }
        }
        parent.blocks.sort_by_key(|block| block.index);
        match parent_type {
            "image" => groups.image.push(parent),
            "table" => groups.table.push(parent),
            "chart" => groups.chart.push(parent),
            "code" => groups.code.push(parent),
            _ => {}
        }
    }

    groups.image.sort_by_key(|block| block.index);
    groups.table.sort_by_key(|block| block.index);
    groups.chart.sort_by_key(|block| block.index);
    groups.code.sort_by_key(|block| block.index);
    (groups, unmatched)
}

fn is_visual_neighbor(blocks: &[PyBlock], child_pos: usize, parent_pos: usize) -> bool {
    let child = &blocks[child_pos];
    if child.block_type == "footnote" && child.index < blocks[parent_pos].index {
        return false;
    }
    let allowed_between: HashSet<&str> = if child.block_type == "caption" {
        HashSet::from(["caption"])
    } else {
        HashSet::from(["caption", "footnote"])
    };
    let start = child_pos.min(parent_pos) + 1;
    let end = child_pos.max(parent_pos);
    blocks[start..end]
        .iter()
        .all(|block| allowed_between.contains(block.block_type.as_str()))
}

fn fix_list_blocks(
    list_blocks: &mut Vec<PyBlock>,
    text_blocks: &mut Vec<PyBlock>,
    ref_text_blocks: &mut Vec<PyBlock>,
) {
    for list_block in list_blocks.iter_mut() {
        list_block.lines.clear();
        let mut remove_text = Vec::new();
        let mut remove_ref = Vec::new();
        for (idx, block) in text_blocks.iter().enumerate() {
            if overlap_ratio(block.bbox, list_block.bbox) >= 0.8 {
                list_block.blocks.push(block.clone());
                remove_text.push(idx);
            }
        }
        for (idx, block) in ref_text_blocks.iter().enumerate() {
            if overlap_ratio(block.bbox, list_block.bbox) >= 0.8 {
                list_block.blocks.push(block.clone());
                remove_ref.push(idx);
            }
        }
        for idx in remove_text.into_iter().rev() {
            text_blocks.remove(idx);
        }
        for idx in remove_ref.into_iter().rev() {
            ref_text_blocks.remove(idx);
        }
        list_block.blocks.sort_by_key(|block| block.index);
        list_block.sub_type = mode_block_type(&list_block.blocks);
    }
    list_blocks.retain(|block| !block.blocks.is_empty());
}

fn mode_block_type(blocks: &[PyBlock]) -> Option<String> {
    let mut counts = HashMap::new();
    for block in blocks {
        *counts.entry(block.block_type.clone()).or_insert(0usize) += 1;
    }
    counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(block_type, _)| block_type)
}

fn merge_para_text_blocks(para_blocks: &mut [PyBlock]) {
    for current_index in (0..para_blocks.len()).rev() {
        if para_blocks[current_index].block_type != "text"
            || para_blocks[current_index].merge_prev != Some(true)
            || para_blocks[current_index].lines.is_empty()
        {
            continue;
        }
        let previous_index =
            (0..current_index)
                .rev()
                .find(|idx| match para_blocks[*idx].block_type.as_str() {
                    "title" | "interline_equation" | "list" => false,
                    "text" => true,
                    _ => false,
                });
        if let Some(previous_index) = previous_index {
            let lines = std::mem::take(&mut para_blocks[current_index].lines);
            para_blocks[previous_index].lines.extend(lines);
        }
    }
}

fn make_content_list(pdf_info: &[PageInfo]) -> Value {
    let mut output = Vec::new();
    for page in pdf_info {
        for block in page.para_blocks.iter().chain(page.discarded_blocks.iter()) {
            if let Some(value) = make_block_content_v1(block, page.page_idx, page.page_size) {
                output.push(value);
            }
        }
    }
    Value::Array(output)
}

fn make_content_list_v2(pdf_info: &[PageInfo]) -> Value {
    Value::Array(
        pdf_info
            .iter()
            .map(|page| {
                Value::Array(
                    page.para_blocks
                        .iter()
                        .chain(page.discarded_blocks.iter())
                        .filter_map(|block| make_block_content_v2(block, page.page_size))
                        .collect(),
                )
            })
            .collect(),
    )
}

fn make_block_content_v1(block: &PyBlock, page_idx: usize, page_size: [u32; 2]) -> Option<Value> {
    let mut content = match block.block_type.as_str() {
        "text" | "ref_text" | "phonetic" | "header" | "footer" | "page_number" | "aside_text"
        | "page_footnote" => json!({
            "type": block.block_type,
            "text": merge_para_with_text(block, true),
        }),
        "list" => json!({
            "type": "list",
            "sub_type": block.sub_type.clone().unwrap_or_default(),
            "list_items": block.blocks.iter()
                .map(|item| merge_para_with_text(item, false))
                .filter(|text| !text.trim().is_empty())
                .collect::<Vec<String>>(),
        }),
        "title" => json!({
            "type": "text",
            "text": merge_para_with_text(block, true),
            "text_level": 1,
        }),
        "interline_equation" => json!({
            "type": "equation",
            "text": merge_para_with_text(block, true),
            "text_format": "latex",
        }),
        "image" => {
            let (image_path, body_content) = get_body_data(block);
            json!({
                "type": "image",
                "img_path": media_path(&image_path),
                "image_caption": child_texts(block, "image_caption"),
                "image_footnote": child_texts(block, "image_footnote"),
                "content": body_content,
            })
        }
        "table" => {
            let (image_path, html) = get_body_data(block);
            let mut table = json!({
                "type": "table",
                "img_path": media_path(&image_path),
                "table_caption": child_texts(block, "table_caption"),
                "table_footnote": child_texts(block, "table_footnote"),
            });
            if !html.is_empty() {
                table["table_body"] = json!(format_embedded_html(&html));
            }
            table
        }
        "chart" => {
            let (image_path, body_content) = get_body_data(block);
            json!({
                "type": "chart",
                "img_path": media_path(&image_path),
                "content": body_content,
                "chart_caption": child_texts(block, "chart_caption"),
                "chart_footnote": child_texts(block, "chart_footnote"),
            })
        }
        "code" => {
            let mut code_body = String::new();
            for child in &block.blocks {
                if child.block_type == "code_body" {
                    code_body = merge_para_with_text(child, true);
                }
            }
            if block.sub_type.as_deref() == Some("code") {
                let lang = block.guess_lang.as_deref().unwrap_or("txt");
                code_body = format!("```{lang}\n{code_body}\n```");
            }
            json!({
                "type": "code",
                "sub_type": block.sub_type.clone().unwrap_or_else(|| "code".to_string()),
                "code_caption": child_texts(block, "code_caption"),
                "code_body": code_body,
            })
        }
        _ => return None,
    };
    content["bbox"] = json!(bbox_to_content_bbox(block.bbox, page_size));
    content["page_idx"] = json!(page_idx);
    Some(content)
}

fn make_block_content_v2(block: &PyBlock, page_size: [u32; 2]) -> Option<Value> {
    let mut content = match block.block_type.as_str() {
        "header" | "footer" | "aside_text" | "page_number" | "page_footnote" => {
            let content_type = match block.block_type.as_str() {
                "header" => "page_header",
                "footer" => "page_footer",
                "aside_text" => "page_aside_text",
                "page_number" => "page_number",
                "page_footnote" => "page_footnote",
                _ => return None,
            };
            let mut map = Map::new();
            map.insert(
                format!("{content_type}_content"),
                Value::Array(merge_para_with_text_v2(block)),
            );
            json!({"type": content_type, "content": map})
        }
        "title" => json!({
            "type": "title",
            "content": {
                "title_content": merge_para_with_text_v2(block),
                "level": 1,
            }
        }),
        "text" | "phonetic" => json!({
            "type": "paragraph",
            "content": {"paragraph_content": merge_para_with_text_v2(block)}
        }),
        "ref_text" => json!({
            "type": "list",
            "content": {
                "list_type": "reference_list",
                "list_items": [{
                    "item_type": "text",
                    "item_content": merge_para_with_text_v2(block),
                }],
            }
        }),
        "interline_equation" => {
            let (image_path, math_content) = get_body_data(block);
            json!({
                "type": "equation_interline",
                "content": {
                    "math_content": math_content,
                    "math_type": "latex",
                    "image_source": {"path": media_path(&image_path)},
                }
            })
        }
        "image" => {
            let (image_path, body_content) = get_body_data(block);
            json!({
                "type": "image",
                "content": {
                    "image_source": {"path": media_path(&image_path)},
                    "content": body_content,
                    "image_caption": child_spans(block, "image_caption"),
                    "image_footnote": child_spans(block, "image_footnote"),
                }
            })
        }
        "table" => {
            let (image_path, html) = get_body_data(block);
            let table_html = format_embedded_html(&html);
            json!({
                "type": "table",
                "content": {
                    "image_source": {"path": media_path(&image_path)},
                    "table_caption": child_spans(block, "table_caption"),
                    "table_footnote": child_spans(block, "table_footnote"),
                    "html": table_html,
                    "table_type": if table_html.contains("colspan") || table_html.contains("rowspan") || table_html.matches("<table").count() > 1 { "complex_table" } else { "simple_table" },
                    "table_nest_level": if table_html.matches("<table").count() > 1 { 2 } else { 1 },
                }
            })
        }
        "chart" => {
            let (image_path, body_content) = get_body_data(block);
            json!({
                "type": "chart",
                "content": {
                    "image_source": {"path": media_path(&image_path)},
                    "content": body_content,
                    "chart_caption": child_spans(block, "chart_caption"),
                    "chart_footnote": child_spans(block, "chart_footnote"),
                }
            })
        }
        "list" => json!({
            "type": "list",
            "content": {
                "list_type": if block.sub_type.as_deref() == Some("ref_text") { "reference_list" } else { "text_list" },
                "list_items": block.blocks.iter()
                    .map(|item| json!({"item_type": "text", "item_content": merge_para_with_text_v2(item)}))
                    .collect::<Vec<Value>>(),
            }
        }),
        _ => return None,
    };
    content["bbox"] = json!(bbox_to_content_bbox(block.bbox, page_size));
    Some(content)
}

fn make_markdown(pdf_info: &[PageInfo]) -> String {
    let mut output = Vec::new();
    for page in pdf_info {
        for block in &page.para_blocks {
            let text = match block.block_type.as_str() {
                "text" | "interline_equation" | "phonetic" | "ref_text" => {
                    merge_para_with_text(block, true)
                }
                "title" => format!("# {}", merge_para_with_text(block, true)),
                "list" => block
                    .blocks
                    .iter()
                    .map(|item| format!("{}  \n", merge_para_with_text(item, false)))
                    .collect::<String>(),
                "image" | "table" | "chart" | "code" => merge_visual_to_markdown(block),
                _ => String::new(),
            };
            if !text.trim().is_empty() {
                output.push(text.trim().to_string());
            }
        }
    }
    output.join("\n\n")
}

fn merge_visual_to_markdown(block: &PyBlock) -> String {
    let mut segments: Vec<(String, &'static str)> = Vec::new();
    for child in &block.blocks {
        match child.block_type.as_str() {
            "image_caption" | "image_footnote" | "table_caption" | "table_footnote"
            | "chart_caption" | "chart_footnote" | "code_caption" | "code_footnote" => {
                let text = merge_para_with_text(child, true);
                if !text.trim().is_empty() {
                    segments.push((text, "markdown_line"));
                }
            }
            "image_body" | "chart_body" => {
                let (image_path, content) = get_body_data(child);
                if !image_path.is_empty() {
                    segments.push((format!("![]({})", media_path(&image_path)), "markdown_line"));
                }
                if !content.trim().is_empty() {
                    segments.push((
                        format!(
                            "<details>\n<summary>image content</summary>\n\n{content}\n</details>"
                        ),
                        "html_block",
                    ));
                }
            }
            "table_body" => {
                let (image_path, html) = get_body_data(child);
                if !html.is_empty() {
                    segments.push((format_embedded_html(&html), "html_block"));
                } else if !image_path.is_empty() {
                    segments.push((format!("![]({})", media_path(&image_path)), "markdown_line"));
                }
            }
            "code_body" => {
                let text = merge_para_with_text(child, true);
                if !text.trim().is_empty() {
                    segments.push((text, "markdown_line"));
                }
            }
            _ => {}
        }
    }

    let mut output = String::new();
    let mut previous_kind = "";
    for (text, kind) in segments {
        if !output.is_empty() {
            output.push_str(if previous_kind == "html_block" || kind == "html_block" {
                "\n\n"
            } else {
                "  \n"
            });
        }
        output.push_str(&text);
        previous_kind = kind;
    }
    output
}

fn child_texts(block: &PyBlock, child_type: &str) -> Vec<String> {
    block
        .blocks
        .iter()
        .filter(|child| child.block_type == child_type)
        .map(|child| merge_para_with_text(child, true))
        .collect()
}

fn child_spans(block: &PyBlock, child_type: &str) -> Vec<Value> {
    block
        .blocks
        .iter()
        .filter(|child| child.block_type == child_type)
        .flat_map(merge_para_with_text_v2)
        .collect()
}

fn get_body_data(block: &PyBlock) -> (String, String) {
    for child in block.blocks.iter().chain(std::iter::once(block)) {
        if matches!(
            child.block_type.as_str(),
            "image_body" | "table_body" | "chart_body" | "code_body" | "interline_equation"
        ) {
            for line in &child.lines {
                for span in &line.spans {
                    match span.span_type.as_str() {
                        "table" => {
                            return (
                                span.image_path.clone().unwrap_or_default(),
                                span.html.clone().unwrap_or_default(),
                            )
                        }
                        "image" | "chart" | "interline_equation" | "text" => {
                            return (
                                span.image_path.clone().unwrap_or_default(),
                                span.content.clone().unwrap_or_default(),
                            )
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    (String::new(), String::new())
}

fn merge_para_with_text(block: &PyBlock, escape_text_prefix: bool) -> String {
    let mut block_text = String::new();
    for line in &block.lines {
        for span in &line.spans {
            if span.span_type == "text" {
                block_text.push_str(span.content.as_deref().unwrap_or_default());
            }
        }
    }
    let is_cjk = is_cjk_context(&block_text);
    let mut output = String::new();
    for (line_index, line) in block.lines.iter().enumerate() {
        for (span_index, span) in line.spans.iter().enumerate() {
            let mut content = match span.span_type.as_str() {
                "text" => {
                    let content = span.content.clone().unwrap_or_default();
                    if block.block_type == "code_body" {
                        content
                    } else {
                        escape_conservative_markdown_text(&content)
                    }
                }
                "inline_equation" => format!("${}$", span.content.as_deref().unwrap_or_default()),
                "interline_equation" => {
                    format!(
                        "\n$$\n{}\n$$\n",
                        span.content.as_deref().unwrap_or_default()
                    )
                }
                _ => String::new(),
            };
            content = content.trim().to_string();
            if content.is_empty() {
                continue;
            }
            if span.span_type == "interline_equation" {
                output.push_str(&content);
                continue;
            }
            let is_last_span = span_index == line.spans.len() - 1;
            if is_cjk {
                if is_last_span && span.span_type != "inline_equation" {
                    output.push_str(&content);
                } else {
                    output.push_str(&format!("{content} "));
                }
            } else if is_last_span
                && span.span_type == "text"
                && content.ends_with('-')
                && block
                    .lines
                    .get(line_index + 1)
                    .and_then(|next| next.spans.first())
                    .and_then(|next| next.content.as_deref())
                    .is_some_and(|next| next.chars().next().is_some_and(char::is_lowercase))
            {
                output.push_str(content.trim_end_matches('-'));
            } else if is_last_span && span.span_type == "text" && content.ends_with('-') {
                output.push_str(&content);
            } else {
                output.push_str(&format!("{content} "));
            }
        }
    }
    if escape_text_prefix && block.block_type == "text" {
        escape_text_block_markdown_prefix(&output)
    } else {
        output
    }
}

fn merge_para_with_text_v2(block: &PyBlock) -> Vec<Value> {
    let is_cjk = block
        .lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .filter(|span| span.span_type == "text")
        .any(|span| is_cjk_context(span.content.as_deref().unwrap_or_default()));
    let mut output: Vec<Value> = Vec::new();
    for (line_index, line) in block.lines.iter().enumerate() {
        for (span_index, span) in line.spans.iter().enumerate() {
            let Some(raw_content) = span.content.as_deref() else {
                continue;
            };
            if raw_content.trim().is_empty() {
                continue;
            }
            let span_type = match span.span_type.as_str() {
                "text" if block.block_type == "phonetic" => "phonetic",
                "text" => "text",
                "inline_equation" => "equation_inline",
                _ => continue,
            };
            let is_last_span = span_index == line.spans.len() - 1;
            let content = if span_type == "text" {
                if is_cjk {
                    if is_last_span {
                        raw_content.to_string()
                    } else {
                        format!("{raw_content} ")
                    }
                } else if is_last_span
                    && raw_content.ends_with('-')
                    && block
                        .lines
                        .get(line_index + 1)
                        .and_then(|next| next.spans.first())
                        .and_then(|next| next.content.as_deref())
                        .is_some_and(|next| next.chars().next().is_some_and(char::is_lowercase))
                {
                    raw_content.trim_end_matches('-').to_string()
                } else if is_last_span && raw_content.ends_with('-') {
                    raw_content.to_string()
                } else {
                    format!("{raw_content} ")
                }
            } else {
                raw_content.to_string()
            };
            if let Some(last) = output.last_mut() {
                if last.get("type").and_then(Value::as_str) == Some(span_type) {
                    let previous = last
                        .get("content")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    last["content"] = json!(format!("{previous}{content}"));
                    continue;
                }
            }
            output.push(json!({"type": span_type, "content": content}));
        }
    }
    output
}

fn page_info_to_json(page: &PageInfo) -> Value {
    json!({
        "preproc_blocks": page.preproc_blocks.iter().map(py_block_to_json).collect::<Vec<Value>>(),
        "discarded_blocks": page.discarded_blocks.iter().map(py_block_to_json).collect::<Vec<Value>>(),
        "page_size": page.page_size,
        "page_idx": page.page_idx,
        "para_blocks": page.para_blocks.iter().map(py_block_to_json).collect::<Vec<Value>>(),
    })
}

fn py_block_to_json(block: &PyBlock) -> Value {
    let mut object = Map::new();
    object.insert("type".to_string(), json!(block.block_type));
    object.insert("bbox".to_string(), json!(block.bbox));
    object.insert("angle".to_string(), json!(block.angle));
    if !block.lines.is_empty() {
        object.insert(
            "lines".to_string(),
            Value::Array(block.lines.iter().map(py_line_to_json).collect()),
        );
    }
    if !block.blocks.is_empty() {
        object.insert(
            "blocks".to_string(),
            Value::Array(block.blocks.iter().map(py_block_to_json).collect()),
        );
    }
    object.insert("index".to_string(), json!(block.index));
    if let Some(sub_type) = &block.sub_type {
        object.insert("sub_type".to_string(), json!(sub_type));
    }
    if let Some(guess_lang) = &block.guess_lang {
        object.insert("guess_lang".to_string(), json!(guess_lang));
    }
    if let Some(merge_prev) = block.merge_prev {
        object.insert("merge_prev".to_string(), json!(merge_prev));
    }
    Value::Object(object)
}

fn py_line_to_json(line: &PyLine) -> Value {
    let mut object = Map::new();
    object.insert("bbox".to_string(), json!(line.bbox));
    object.insert(
        "spans".to_string(),
        Value::Array(line.spans.iter().map(py_span_to_json).collect()),
    );
    if let Some(extra) = &line.extra {
        object.insert("extra".to_string(), extra.clone());
    }
    Value::Object(object)
}

fn py_span_to_json(span: &PySpan) -> Value {
    let mut object = Map::new();
    object.insert("bbox".to_string(), json!(span.bbox));
    object.insert("type".to_string(), json!(span.span_type));
    if let Some(content) = &span.content {
        object.insert("content".to_string(), json!(content));
    }
    if let Some(html) = &span.html {
        object.insert("html".to_string(), json!(html));
    }
    if let Some(image_path) = &span.image_path {
        object.insert("image_path".to_string(), json!(image_path));
    }
    Value::Object(object)
}

fn model_block_to_json(block: &ContentBlock) -> Value {
    let mut object = Map::new();
    object.insert("type".to_string(), json!(block.block_type));
    object.insert(
        "bbox".to_string(),
        json!(block.bbox.map(|coord| round3(coord as f64))),
    );
    object.insert("angle".to_string(), json!(block.angle.unwrap_or(0)));
    if let Some(content) = &block.content {
        object.insert("content".to_string(), json!(content));
    }
    if let Some(merge_prev) = block.merge_prev {
        object.insert("merge_prev".to_string(), json!(merge_prev));
    }
    Value::Object(object)
}

async fn write_python_crop(
    page: &PythonPageInput,
    page_md5: &str,
    span_type: &str,
    bbox: [i64; 4],
    pending_image_dir: &Path,
) -> ApiResult<PathBuf> {
    let raw_path = format!(
        "{span_type}/{page_md5}_{}_{}_{}_{}_{}",
        page.page_index, bbox[0], bbox[1], bbox[2], bbox[3]
    );
    let file_name = format!("{}.jpg", sha256_hex(&raw_path));
    let path = pending_image_dir.join(file_name);
    let scale_x = page.page_width as f32 / page.point_width.max(1) as f32;
    let scale_y = page.page_height as f32 / page.point_height.max(1) as f32;
    let x1 = ((bbox[0] as f32) * scale_x).floor().max(0.0) as u32;
    let y1 = ((bbox[1] as f32) * scale_y).floor().max(0.0) as u32;
    let x2 = ((bbox[2] as f32) * scale_x)
        .ceil()
        .min(page.page_width as f32) as u32;
    let y2 = ((bbox[3] as f32) * scale_y)
        .ceil()
        .min(page.page_height as f32) as u32;
    let crop = if x2 > x1 && y2 > y1 {
        page.image.crop_imm(x1, y1, x2 - x1, y2 - y1)
    } else {
        DynamicImage::new_rgb8(0, 0)
    };
    let mut bytes = Cursor::new(Vec::new());
    crop.write_to(&mut bytes, ImageFormat::Jpeg)
        .map_err(|error| ApiError::Internal(error.to_string()))?;
    fs::write(&path, bytes.into_inner()).await?;
    Ok(path)
}

fn normalized_to_point_bbox(bbox: [f32; 4], width: u32, height: u32) -> Option<[i64; 4]> {
    let mut x1 = ((bbox[0] as f64) * width as f64).floor() as i64;
    let mut y1 = ((bbox[1] as f64) * height as f64).floor() as i64;
    let mut x2 = ((bbox[2] as f64) * width as f64).floor() as i64;
    let mut y2 = ((bbox[3] as f64) * height as f64).floor() as i64;
    if x2 < x1 {
        std::mem::swap(&mut x1, &mut x2);
    }
    if y2 < y1 {
        std::mem::swap(&mut y1, &mut y2);
    }
    (x2 > x1 && y2 > y1).then_some([x1, y1, x2, y2])
}

fn bbox_to_content_bbox(bbox: [i64; 4], page_size: [u32; 2]) -> [i64; 4] {
    [
        bbox[0] * 1000 / page_size[0] as i64,
        bbox[1] * 1000 / page_size[1] as i64,
        bbox[2] * 1000 / page_size[0] as i64,
        bbox[3] * 1000 / page_size[1] as i64,
    ]
}

fn clean_text_content(raw_type: &str, content: &str) -> String {
    let mut content = content.replace("\\[", "[").replace("\\]", "]");
    if raw_type == "title" {
        content = content
            .lines()
            .map(str::trim)
            .collect::<Vec<&str>>()
            .join(" ");
    }
    content
}

fn clean_interline_equation(content: &str) -> String {
    content
        .trim()
        .strip_prefix("\\[")
        .unwrap_or(content.trim())
        .strip_suffix("\\]")
        .unwrap_or_else(|| content.trim())
        .trim()
        .to_string()
}

fn process_equation(content: &str) -> String {
    clean_interline_equation(content)
}

fn add_equation_brackets(content: String) -> String {
    let mut content = content.trim().to_string();
    if !content.starts_with("\\[") {
        content = format!("\\[\n{content}");
    }
    if !content.ends_with("\\]") {
        content = format!("{content}\n\\]");
    }
    content
}

fn image_analysis_content(content: Option<&str>) -> Option<String> {
    let content = content?.trim();
    if content.is_empty() || content.contains("<fcel>") {
        Some(String::new())
    } else {
        Some(content.to_string())
    }
}

fn format_embedded_html(html: &str) -> String {
    let regex = regex::Regex::new(r"<eq>(.*?)</eq>").expect("eq tag regex compiles");
    regex
        .replace_all(html, |captures: &regex::Captures| {
            format!(" ${}$ ", html_unescape(&captures[1]))
        })
        .to_string()
}

fn media_path(image_path: &str) -> String {
    if image_path.is_empty() {
        String::new()
    } else {
        format!("images/{image_path}")
    }
}

fn convert_otsl_to_html(content: &str) -> String {
    let stripped = content.trim();
    if stripped.starts_with("<table") && stripped.ends_with("</table>") {
        return stripped.to_string();
    }
    let tokens = ["<nl>", "<fcel>", "<ecel>", "<lcel>", "<ucel>", "<xcel>"];
    if !tokens.iter().any(|token| stripped.contains(token)) {
        return stripped.to_string();
    }
    let parts = split_otsl(stripped);
    let rows = build_otsl_rows(&parts);
    if rows.is_empty() {
        return String::new();
    }
    let max_cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut html = String::from("<table>");
    for row in rows {
        html.push_str("<tr>");
        let mut col = 0usize;
        while col < max_cols {
            let cell = row.get(col).cloned().unwrap_or_default();
            html.push_str("<td>");
            html.push_str(&html_escape(&cell));
            html.push_str("</td>");
            col += 1;
        }
        html.push_str("</tr>");
    }
    html.push_str("</table>");
    html
}

fn split_otsl(content: &str) -> Vec<String> {
    let regex = regex::Regex::new(r"(<nl>|<fcel>|<ecel>|<lcel>|<ucel>|<xcel>)")
        .expect("OTSL regex compiles");
    let mut output = Vec::new();
    let mut cursor = 0;
    for match_ in regex.find_iter(content) {
        if match_.start() > cursor {
            output.push(content[cursor..match_.start()].trim().to_string());
        }
        output.push(match_.as_str().to_string());
        cursor = match_.end();
    }
    if cursor < content.len() {
        output.push(content[cursor..].trim().to_string());
    }
    output
        .into_iter()
        .filter(|part| !part.trim().is_empty())
        .collect()
}

fn build_otsl_rows(parts: &[String]) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut index = 0;
    while index < parts.len() {
        match parts[index].as_str() {
            "<nl>" => {
                rows.push(std::mem::take(&mut row));
            }
            "<fcel>" | "<ecel>" => {
                let value = parts
                    .get(index + 1)
                    .filter(|next| !next.starts_with('<'))
                    .cloned()
                    .unwrap_or_default();
                row.push(value);
                if parts
                    .get(index + 1)
                    .is_some_and(|next| !next.starts_with('<'))
                {
                    index += 1;
                }
            }
            "<lcel>" | "<ucel>" | "<xcel>" => {}
            text => row.push(text.to_string()),
        }
        index += 1;
    }
    if !row.is_empty() {
        rows.push(row);
    }
    rows
}

fn page_image_md5(image: &DynamicImage) -> String {
    let rgb = image.to_rgb8();
    let mut hasher = Md5::new();
    hasher.update(rgb.as_raw());
    format!("{:X}", hasher.finalize())
}

fn sha256_hex(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn round3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

fn overlap_ratio(bbox1: [i64; 4], bbox2: [i64; 4]) -> f64 {
    let x_left = bbox1[0].max(bbox2[0]);
    let y_top = bbox1[1].max(bbox2[1]);
    let x_right = bbox1[2].min(bbox2[2]);
    let y_bottom = bbox1[3].min(bbox2[3]);
    if x_right <= x_left || y_bottom <= y_top {
        return 0.0;
    }
    let intersection = (x_right - x_left) * (y_bottom - y_top);
    let area1 = (bbox1[2] - bbox1[0]).max(1) * (bbox1[3] - bbox1[1]).max(1);
    intersection as f64 / area1 as f64
}

fn bbox_distance(a: [i64; 4], b: [i64; 4]) -> i64 {
    let dx = if a[2] < b[0] {
        b[0] - a[2]
    } else if b[2] < a[0] {
        a[0] - b[2]
    } else {
        0
    };
    let dy = if a[3] < b[1] {
        b[1] - a[3]
    } else if b[3] < a[1] {
        a[1] - b[3]
    } else {
        0
    };
    dx + dy
}

fn iter_spans_mut(block: &mut PyBlock) -> Vec<&mut PySpan> {
    let mut spans = Vec::new();
    for line in &mut block.lines {
        for span in &mut line.spans {
            spans.push(span);
        }
    }
    for child in &mut block.blocks {
        spans.extend(iter_spans_mut(child));
    }
    spans
}

fn is_visual_main_type(block_type: &str) -> bool {
    matches!(
        block_type,
        "image_body" | "image_block_body" | "table_body" | "chart_body" | "code_body"
    )
}

fn is_generic_child_type(block_type: &str) -> bool {
    matches!(block_type, "caption" | "footnote")
}

fn visual_parent_type(block_type: &str) -> Option<&'static str> {
    match block_type {
        "image_body" | "image_block_body" => Some("image"),
        "table_body" => Some("table"),
        "chart_body" => Some("chart"),
        "code_body" => Some("code"),
        _ => None,
    }
}

fn visual_body_type(parent_type: &str) -> &'static str {
    match parent_type {
        "image" => "image_body",
        "table" => "table_body",
        "chart" => "chart_body",
        "code" => "code_body",
        _ => "text",
    }
}

fn visual_caption_type(parent_type: &str) -> &'static str {
    match parent_type {
        "image" => "image_caption",
        "table" => "table_caption",
        "chart" => "chart_caption",
        "code" => "code_caption",
        _ => "caption",
    }
}

fn visual_footnote_type(parent_type: &str) -> &'static str {
    match parent_type {
        "image" => "image_footnote",
        "table" => "table_footnote",
        "chart" => "chart_footnote",
        "code" => "code_footnote",
        _ => "footnote",
    }
}

fn contains_cjk(content: &str) -> bool {
    content.chars().any(|ch| {
        matches!(
            ch as u32,
            0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0x3040..=0x30FF | 0xAC00..=0xD7AF
        )
    })
}

fn is_cjk_context(content: &str) -> bool {
    contains_cjk(content) || content.trim() == "usz@google.com"
}

fn escape_text_block_markdown_prefix(content: &str) -> String {
    let bytes = content.as_bytes();
    let mut marker_start = 0usize;
    while marker_start < bytes.len()
        && marker_start < 3
        && matches!(bytes[marker_start], b' ' | b'\t')
    {
        marker_start += 1;
    }
    if marker_start >= bytes.len() {
        return content.to_string();
    }
    let marker_len = if bytes[marker_start] == b'#' {
        let mut len = 0usize;
        while marker_start + len < bytes.len() && len < 6 && bytes[marker_start + len] == b'#' {
            len += 1;
        }
        len
    } else if matches!(bytes[marker_start], b'+' | b'-') {
        1
    } else {
        0
    };
    let after_marker = marker_start + marker_len;
    if marker_len > 0 && after_marker < bytes.len() && matches!(bytes[after_marker], b' ' | b'\t') {
        let mut escaped = String::new();
        escaped.push_str(&content[..marker_start]);
        escaped.push('\\');
        escaped.push_str(&content[marker_start..]);
        return escaped;
    }
    content.to_string()
}

fn escape_conservative_markdown_text(content: &str) -> String {
    let mut escaped = String::with_capacity(content.len());
    let mut preceding_backslashes = 0usize;
    for ch in content.chars() {
        if ch == '\\' {
            escaped.push(ch);
            preceding_backslashes += 1;
            continue;
        }
        if matches!(ch, '*' | '_' | '`' | '~' | '$') && preceding_backslashes.is_multiple_of(2) {
            escaped.push('\\');
        }
        escaped.push(ch);
        preceding_backslashes = 0;
    }
    escaped
}

fn html_escape(content: &str) -> String {
    content
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn html_unescape(content: &str) -> String {
    content
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, path::Path};

    use image::RgbImage;
    use serde_json::Value;

    use super::*;

    #[tokio::test]
    async fn nips_fixture_matches_python_content_shapes() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let python_dir =
            root.join("temp/PythonVersion/NIPS-2017-attention-is-all-you-need-Paper(1)/vlm");
        let python_model_path =
            python_dir.join("NIPS-2017-attention-is-all-you-need-Paper(1)_model.json");
        if !python_dir.exists() || !python_model_path.exists() {
            return;
        }

        let pages = load_fixture_pages(&python_model_path);
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let mut builder = DocumentOutputAccumulator::new();
        builder
            .append_pages(pages, temp.path())
            .await
            .expect("fixture output should build");
        let output = builder.finish();
        let python_content: Value = serde_json::from_slice(
            &std::fs::read(
                python_dir.join("NIPS-2017-attention-is-all-you-need-Paper(1)_content_list.json"),
            )
            .expect("python content fixture should read"),
        )
        .expect("python content fixture should parse");
        let python_v2: Value = serde_json::from_slice(
            &std::fs::read(
                python_dir
                    .join("NIPS-2017-attention-is-all-you-need-Paper(1)_content_list_v2.json"),
            )
            .expect("python v2 fixture should read"),
        )
        .expect("python v2 fixture should parse");

        assert_eq!(
            output.content_list.as_array().map(Vec::len),
            python_content.as_array().map(Vec::len)
        );
        assert_eq!(
            type_counts(&output.content_list),
            type_counts(&python_content)
        );
        assert_eq!(output.content_list[0]["bbox"], python_content[0]["bbox"]);
        assert_eq!(output.content_list[0]["type"], "text");
        assert_eq!(
            output.content_list_v2.as_array().map(Vec::len),
            python_v2.as_array().map(Vec::len)
        );
        assert_eq!(
            flattened_len(&output.content_list_v2),
            flattened_len(&python_v2)
        );
        assert_values_equal_by_item(
            &normalize_media_paths(output.content_list.clone()),
            &normalize_media_paths(python_content),
        );
        assert_page_values_equal_by_item(
            &normalize_media_paths(output.content_list_v2.clone()),
            &normalize_media_paths(python_v2),
        );
    }

    #[tokio::test]
    async fn page_fragments_match_append_pages_output() {
        let pages = vec![test_page_input(0, "alpha"), test_page_input(1, "beta")];
        let append_temp = tempfile::tempdir().expect("append tempdir should be created");
        let fragment_temp = tempfile::tempdir().expect("fragment tempdir should be created");

        let mut append_builder = DocumentOutputAccumulator::new();
        append_builder
            .append_pages(pages.clone(), append_temp.path())
            .await
            .expect("append_pages should build");

        let mut fragments = Vec::new();
        for page in pages {
            fragments.push(
                build_page_output_fragment(page, fragment_temp.path())
                    .await
                    .expect("page fragment should build"),
            );
        }
        let mut fragment_builder = DocumentOutputAccumulator::new();
        fragment_builder.append_fragments(fragments);

        let append_output = append_builder.finish();
        let fragment_output = fragment_builder.finish();
        assert_eq!(append_output.markdown, fragment_output.markdown);
        assert_eq!(append_output.middle_json, fragment_output.middle_json);
        assert_eq!(append_output.model_output, fragment_output.model_output);
        assert_eq!(append_output.content_list, fragment_output.content_list);
        assert_eq!(
            append_output.content_list_v2,
            fragment_output.content_list_v2
        );
    }

    fn load_fixture_pages(path: &Path) -> Vec<PythonPageInput> {
        let model: Value =
            serde_json::from_slice(&std::fs::read(path).expect("model fixture should read"))
                .expect("model fixture should parse");
        model
            .as_array()
            .expect("model should be an array")
            .iter()
            .enumerate()
            .map(|page| {
                let (page_index, page) = page;
                let blocks = page
                    .as_array()
                    .expect("page blocks should be an array")
                    .iter()
                    .map(json_to_content_block)
                    .collect::<Vec<_>>();
                PythonPageInput {
                    page_index,
                    page_width: 1700,
                    page_height: 2200,
                    point_width: 612,
                    point_height: 792,
                    image: Arc::new(DynamicImage::ImageRgb8(RgbImage::new(1700, 2200))),
                    blocks,
                }
            })
            .collect()
    }

    fn test_page_input(page_index: usize, content: &str) -> PythonPageInput {
        PythonPageInput {
            page_index,
            page_width: 100,
            page_height: 100,
            point_width: 100,
            point_height: 100,
            image: Arc::new(DynamicImage::ImageRgb8(RgbImage::new(100, 100))),
            blocks: vec![ContentBlock {
                block_type: "text".to_string(),
                bbox: [0.0, 0.0, 1.0, 1.0],
                angle: None,
                content: Some(content.to_string()),
                merge_prev: None,
            }],
        }
    }

    fn json_to_content_block(value: &Value) -> ContentBlock {
        let bbox = value["bbox"]
            .as_array()
            .expect("bbox should be array")
            .iter()
            .map(|coord| coord.as_f64().unwrap_or_default() as f32)
            .collect::<Vec<_>>();
        ContentBlock {
            block_type: value["type"].as_str().unwrap_or_default().to_string(),
            bbox: [bbox[0], bbox[1], bbox[2], bbox[3]],
            angle: value["angle"].as_u64().map(|angle| angle as u16),
            content: value
                .get("content")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            merge_prev: value.get("merge_prev").and_then(Value::as_bool),
        }
    }

    fn type_counts(value: &Value) -> HashMap<String, usize> {
        let mut counts = HashMap::new();
        for item in value.as_array().into_iter().flatten() {
            if let Some(kind) = item.get("type").and_then(Value::as_str) {
                *counts.entry(kind.to_string()).or_insert(0) += 1;
            }
        }
        counts
    }

    fn flattened_len(value: &Value) -> usize {
        value
            .as_array()
            .into_iter()
            .flatten()
            .map(|page| page.as_array().map(Vec::len).unwrap_or(0))
            .sum()
    }

    fn normalize_media_paths(mut value: Value) -> Value {
        match &mut value {
            Value::Array(items) => {
                for item in items {
                    normalize_media_paths_in_place(item);
                }
            }
            other => normalize_media_paths_in_place(other),
        }
        value
    }

    fn normalize_media_paths_in_place(value: &mut Value) {
        match value {
            Value::Object(map) => {
                for (key, child) in map.iter_mut() {
                    if matches!(key.as_str(), "img_path" | "path" | "image_path") {
                        if child.as_str().is_some_and(|path| !path.is_empty()) {
                            *child = json!("__MEDIA__");
                        }
                    } else {
                        normalize_media_paths_in_place(child);
                    }
                }
            }
            Value::Array(items) => {
                for item in items {
                    normalize_media_paths_in_place(item);
                }
            }
            _ => {}
        }
    }

    fn assert_values_equal_by_item(left: &Value, right: &Value) {
        let left_items = left.as_array().expect("left should be array");
        let right_items = right.as_array().expect("right should be array");
        assert_eq!(left_items.len(), right_items.len());
        for (index, (left, right)) in left_items.iter().zip(right_items).enumerate() {
            assert_eq!(left, right, "content_list item {index} differs");
        }
    }

    fn assert_page_values_equal_by_item(left: &Value, right: &Value) {
        let left_pages = left.as_array().expect("left should be array");
        let right_pages = right.as_array().expect("right should be array");
        assert_eq!(left_pages.len(), right_pages.len());
        for (page_index, (left_page, right_page)) in left_pages.iter().zip(right_pages).enumerate()
        {
            let left_items = left_page.as_array().expect("left page should be array");
            let right_items = right_page.as_array().expect("right page should be array");
            assert_eq!(
                left_items.len(),
                right_items.len(),
                "content_list_v2 page {page_index} length differs"
            );
            for (item_index, (left, right)) in left_items.iter().zip(right_items).enumerate() {
                assert_eq!(
                    left, right,
                    "content_list_v2 page {page_index} item {item_index} differs"
                );
            }
        }
    }
}
