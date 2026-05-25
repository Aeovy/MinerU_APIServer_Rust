use std::{
    collections::HashSet,
    io::Cursor,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

use futures::{stream, StreamExt};
use image::{imageops, DynamicImage, GenericImageView, ImageFormat, Rgba};
use pdfium_render::prelude::*;
use serde_json::{json, Value};
use tokio::{fs, sync::Semaphore};

use crate::{
    domain::models::{ContentBlock, ParseTask, ParsedDocument, StoredUpload},
    error::{ApiError, ApiResult},
};

use super::client::{
    layout_prompt, layout_sampling_params, prompt_for_block, sampling_params_for_block,
    VlmHttpClient, VlmRequest,
};

const DEFAULT_PDF_IMAGE_DPI: f32 = 200.0;
const LAYOUT_IMAGE_SIZE: u32 = 1036;
const MIN_IMAGE_EDGE: u32 = 28;
const MAX_IMAGE_EDGE_RATIO: f32 = 50.0;

#[derive(Clone)]
pub struct VlmDocumentParser {
    client: Arc<VlmHttpClient>,
    processing_window_size: usize,
    vlm_max_concurrency: usize,
}

struct PageParseResult {
    page_index: usize,
    page_width: u32,
    page_height: u32,
    blocks: Vec<ContentBlock>,
    image_files: Vec<PathBuf>,
}

struct BlockExtractJob {
    block_index: usize,
    block_type: String,
    image_png: Vec<u8>,
    store_image: bool,
}

struct BlockExtractResult {
    block_index: usize,
    content: String,
    image_png: Option<Vec<u8>>,
}

impl VlmDocumentParser {
    pub fn new(
        client: Arc<VlmHttpClient>,
        processing_window_size: usize,
        vlm_max_concurrency: usize,
    ) -> Self {
        Self {
            client,
            processing_window_size,
            vlm_max_concurrency: vlm_max_concurrency.max(1),
        }
    }

    /// Parse all uploads in a task and persist MinerU-compatible result files.
    ///
    /// Inputs:
    /// - `task`: task options and output directory.
    pub async fn parse_task(&self, task: &ParseTask) -> ApiResult<Vec<String>> {
        let mut response_file_names = Vec::new();
        for ((path, stem), suffix) in task
            .uploads
            .iter()
            .zip(task.file_names.iter())
            .zip(task.upload_suffixes.iter())
        {
            let upload = StoredUpload {
                stem: stem.clone(),
                path: path.clone(),
                suffix: suffix.clone(),
            };
            let document = self.parse_upload(task, &upload).await?;
            self.write_document(&task.output_dir, &document).await?;
            response_file_names.push(document.file_name);
        }
        Ok(response_file_names)
    }

    async fn parse_upload(
        &self,
        task: &ParseTask,
        upload: &StoredUpload,
    ) -> ApiResult<ParsedDocument> {
        let pages = self
            .load_pages(upload, task.start_page_id, task.end_page_id)
            .await?;
        let limiter = Arc::new(Semaphore::new(self.vlm_max_concurrency));
        let mut page_results = Vec::new();
        let mut enumerated_pages = pages.into_iter().enumerate();

        loop {
            let page_window = enumerated_pages
                .by_ref()
                .take(self.processing_window_size.max(1))
                .collect::<Vec<_>>();
            if page_window.is_empty() {
                break;
            }

            let unordered_results =
                stream::iter(page_window.into_iter().map(|(page_index, page)| {
                    self.parse_page(task, page_index, page, limiter.clone())
                }))
                .buffer_unordered(self.vlm_max_concurrency)
                .collect::<Vec<_>>()
                .await;
            let mut window_results = unordered_results
                .into_iter()
                .collect::<ApiResult<Vec<PageParseResult>>>()?;
            window_results.sort_by_key(|result| result.page_index);
            page_results.extend(window_results);
        }
        page_results.sort_by_key(|result| result.page_index);

        let mut all_page_blocks = Vec::new();
        let mut markdown_parts = Vec::new();
        let mut content_list = Vec::new();
        let mut content_list_v2_pages = Vec::new();
        let mut image_files = Vec::new();

        for result in page_results {
            let page_index = result.page_index;
            let blocks = result.blocks;
            let PageContent {
                content_list: page_content_list,
                content_list_v2: page_content_list_v2,
                middle_para_blocks,
            } = build_page_content(page_index, &blocks);
            markdown_parts.push(blocks_to_markdown(&blocks));
            content_list.extend(page_content_list);
            content_list_v2_pages.push(Value::Array(page_content_list_v2));
            image_files.extend(result.image_files);
            all_page_blocks.push(json!({
                "preproc_blocks": blocks,
                "discarded_blocks": [],
                "para_blocks": middle_para_blocks,
                "page_size": [result.page_width, result.page_height],
                "page_idx": page_index
            }));
        }

        Ok(ParsedDocument {
            file_name: upload.stem.clone(),
            markdown: markdown_parts.join("\n\n"),
            middle_json: json!({
                "pdf_info": all_page_blocks,
                "_backend": "vlm",
                "_version_name": crate::config::MINERU_VERSION
            }),
            model_output: json!({
                "pages": all_page_blocks
                    .iter()
                    .map(|page| page.get("preproc_blocks").cloned().unwrap_or_else(|| json!([])))
                    .collect::<Vec<Value>>()
            }),
            content_list: Value::Array(content_list),
            content_list_v2: Value::Array(content_list_v2_pages),
            image_files,
        })
    }

    /// Parse one rendered page with MinerU's two-step VLM flow.
    ///
    /// Inputs:
    /// - `task`: request options and output directory.
    /// - `page_index`: zero-based page index in the parsed document.
    /// - `page_image`: rendered PDF page or uploaded image.
    /// - `limiter`: shared per-document VLM request limiter.
    async fn parse_page(
        &self,
        task: &ParseTask,
        page_index: usize,
        page_image: DynamicImage,
        limiter: Arc<Semaphore>,
    ) -> ApiResult<PageParseResult> {
        let priority = Some(page_index as i32);
        let layout_image = encode_png(&prepare_layout_image(&page_image)?)?;
        let layout_output = self
            .predict_with_limit(
                &limiter,
                VlmRequest {
                    server_url: task.server_url.clone(),
                    prompt: layout_prompt().to_string(),
                    image_png: Some(layout_image),
                    sampling_params: layout_sampling_params(),
                    priority,
                },
            )
            .await?;
        let mut blocks = parse_layout_output(&layout_output);
        let image_files = self
            .extract_blocks(task, page_index, &page_image, &mut blocks, limiter)
            .await?;

        Ok(PageParseResult {
            page_index,
            page_width: page_image.width(),
            page_height: page_image.height(),
            blocks,
            image_files,
        })
    }

    /// Extract content for all eligible blocks on one page concurrently.
    ///
    /// Inputs:
    /// - `task`: request options and output directory.
    /// - `page_index`: zero-based page index.
    /// - `page_image`: source page image used for block crops.
    /// - `blocks`: layout blocks to update with recognized content.
    /// - `limiter`: shared per-document VLM request limiter.
    async fn extract_blocks(
        &self,
        task: &ParseTask,
        page_index: usize,
        page_image: &DynamicImage,
        blocks: &mut [ContentBlock],
        limiter: Arc<Semaphore>,
    ) -> ApiResult<Vec<PathBuf>> {
        let skip_types = skip_extract_types(task.image_analysis);
        let mut jobs = Vec::new();
        for (block_index, block) in blocks.iter().enumerate() {
            if skip_types.contains(block.block_type.as_str()) {
                continue;
            }
            let block_image = crop_block_image(page_image, block)?;
            if block_image.width() < 1 || block_image.height() < 1 {
                continue;
            }
            let block_image = resize_by_need(block_image);
            let image_png = encode_png(&block_image)?;
            jobs.push(BlockExtractJob {
                block_index,
                block_type: block.block_type.clone(),
                image_png,
                store_image: matches!(block.block_type.as_str(), "image" | "chart" | "table"),
            });
        }

        let unordered_results =
            stream::iter(jobs.into_iter().map(|job| {
                self.extract_block(task, page_priority(page_index), job, limiter.clone())
            }))
            .buffer_unordered(self.vlm_max_concurrency)
            .collect::<Vec<_>>()
            .await;
        let mut results = unordered_results
            .into_iter()
            .collect::<ApiResult<Vec<BlockExtractResult>>>()?;
        results.sort_by_key(|result| result.block_index);

        let mut image_files = Vec::new();
        for result in results {
            if !result.content.is_empty() {
                blocks[result.block_index].content = Some(result.content);
            }
            if let Some(image_png) = result.image_png {
                let image_path = self
                    .write_result_image_bytes(task, page_index, result.block_index, &image_png)
                    .await?;
                image_files.push(image_path);
            }
        }
        Ok(image_files)
    }

    /// Send one block crop to the VLM backend and preserve optional image bytes.
    ///
    /// Inputs:
    /// - `task`: request options copied from the multipart form.
    /// - `priority`: backend scheduling priority.
    /// - `job`: prepared block image, prompt type, and output-image flag.
    /// - `limiter`: shared per-document VLM request limiter.
    async fn extract_block(
        &self,
        task: &ParseTask,
        priority: Option<i32>,
        job: BlockExtractJob,
        limiter: Arc<Semaphore>,
    ) -> ApiResult<BlockExtractResult> {
        let image_png = job.image_png;
        let result_image_png = job.store_image.then(|| image_png.clone());
        let content = self
            .predict_with_limit(
                &limiter,
                VlmRequest {
                    server_url: task.server_url.clone(),
                    prompt: prompt_for_block(&job.block_type).to_string(),
                    image_png: Some(image_png),
                    sampling_params: sampling_params_for_block(&job.block_type),
                    priority,
                },
            )
            .await?;

        Ok(BlockExtractResult {
            block_index: job.block_index,
            content,
            image_png: result_image_png,
        })
    }

    /// Execute one VLM request under the per-document concurrency limit.
    ///
    /// Inputs:
    /// - `limiter`: semaphore shared by all page and block requests in this document.
    /// - `request`: OpenAI-compatible chat completion payload data.
    async fn predict_with_limit(
        &self,
        limiter: &Arc<Semaphore>,
        request: VlmRequest,
    ) -> ApiResult<String> {
        let _permit = limiter
            .clone()
            .acquire_owned()
            .await
            .map_err(|error| ApiError::Internal(error.to_string()))?;
        self.client.predict(request).await
    }

    async fn write_result_image_bytes(
        &self,
        task: &ParseTask,
        page_index: usize,
        block_index: usize,
        image_png: &[u8],
    ) -> ApiResult<PathBuf> {
        let image_dir = task.output_dir.join("_pending_images");
        fs::create_dir_all(&image_dir).await?;
        let path = image_dir.join(format!("page_{page_index}_block_{block_index}.png"));
        fs::write(&path, image_png).await?;
        Ok(path)
    }

    async fn write_document(&self, output_dir: &Path, document: &ParsedDocument) -> ApiResult<()> {
        let parse_dir = output_dir.join(&document.file_name).join("vlm");
        let images_dir = parse_dir.join("images");
        fs::create_dir_all(&images_dir).await?;
        fs::write(
            parse_dir.join(format!("{}.md", document.file_name)),
            &document.markdown,
        )
        .await?;
        fs::write(
            parse_dir.join(format!("{}_middle.json", document.file_name)),
            serde_json::to_vec_pretty(&document.middle_json)?,
        )
        .await?;
        fs::write(
            parse_dir.join(format!("{}_model.json", document.file_name)),
            serde_json::to_vec_pretty(&document.model_output)?,
        )
        .await?;
        fs::write(
            parse_dir.join(format!("{}_content_list.json", document.file_name)),
            serde_json::to_vec_pretty(&document.content_list)?,
        )
        .await?;
        fs::write(
            parse_dir.join(format!("{}_content_list_v2.json", document.file_name)),
            serde_json::to_vec_pretty(&document.content_list_v2)?,
        )
        .await?;

        for image_file in &document.image_files {
            if let Some(name) = image_file.file_name() {
                fs::copy(image_file, images_dir.join(name)).await?;
            }
        }
        Ok(())
    }

    async fn load_pages(
        &self,
        upload: &StoredUpload,
        start_page_id: usize,
        end_page_id: usize,
    ) -> ApiResult<Vec<DynamicImage>> {
        let bytes = fs::read(&upload.path).await?;
        if upload.suffix == "pdf" {
            let window_size = self.processing_window_size.max(1);
            tokio::task::spawn_blocking(move || {
                render_pdf_pages(&bytes, start_page_id, end_page_id, window_size)
            })
            .await
            .map_err(|error| ApiError::Internal(error.to_string()))?
        } else {
            let image = image::load_from_memory(&bytes)
                .map_err(|error| ApiError::BadRequest(format!("Failed to load image: {error}")))?;
            Ok(vec![image])
        }
    }
}

pub fn parse_layout_output(output: &str) -> Vec<ContentBlock> {
    static LAYOUT_RE: OnceLock<regex::Regex> = OnceLock::new();
    let regex = LAYOUT_RE.get_or_init(|| {
        regex::Regex::new(
            r"(?s)^<\|box_start\|>(\d+)\s+(\d+)\s+(\d+)\s+(\d+)<\|box_end\|><\|ref_start\|>(\w+?)<\|ref_end\|>(?:(<\|rotate_(?:up|right|down|left)\|>))?(.*)$",
        )
        .expect("layout regex must compile")
    });
    split_layout_segments(output)
        .iter()
        .filter_map(|segment| {
            let captures = regex.captures(segment)?;
            let bbox = convert_bbox([
                captures.get(1)?.as_str().parse().ok()?,
                captures.get(2)?.as_str().parse().ok()?,
                captures.get(3)?.as_str().parse().ok()?,
                captures.get(4)?.as_str().parse().ok()?,
            ])?;
            let mut block_type = captures.get(5)?.as_str().to_lowercase();
            if block_type == "unknown" {
                block_type = "image".to_string();
            }
            if block_type == "inline_formula"
                || !allowed_block_types().contains(block_type.as_str())
            {
                return None;
            }
            let angle = captures
                .get(6)
                .and_then(|token| parse_angle(token.as_str()));
            let merge_prev = (block_type == "text").then(|| {
                captures
                    .get(7)
                    .is_some_and(|tail| tail.as_str().contains("txt_contd_tgt"))
            });
            Some(ContentBlock {
                block_type,
                bbox,
                angle,
                content: None,
                merge_prev,
            })
        })
        .collect()
}

fn split_layout_segments(output: &str) -> Vec<String> {
    let marker = "<|box_start|>";
    output
        .split(marker)
        .filter(|segment| !segment.trim().is_empty())
        .map(|segment| {
            let end = segment.find(marker).unwrap_or(segment.len());
            segment[..end].trim()
        })
        .map(|segment| format!("{marker}{segment}"))
        .collect()
}

fn render_pdf_pages(
    bytes: &[u8],
    start_page_id: usize,
    end_page_id: usize,
    processing_window_size: usize,
) -> ApiResult<Vec<DynamicImage>> {
    let pdfium = bind_pdfium()?;
    let document = pdfium
        .load_pdf_from_byte_slice(bytes, None)
        .map_err(|error| ApiError::BadRequest(format!("Failed to open PDF: {error}")))?;
    let page_count = document.pages().len() as usize;
    if page_count == 0 {
        return Ok(Vec::new());
    }
    let start = start_page_id.min(page_count - 1);
    let end = end_page_id.min(page_count - 1);
    let mut images = Vec::new();
    for window_start in (start..=end).step_by(processing_window_size) {
        let window_end = end.min(window_start + processing_window_size - 1);
        for page_index in window_start..=window_end {
            let page = document
                .pages()
                .get(page_index as u16)
                .map_err(|error| ApiError::Internal(error.to_string()))?;
            let width = ((page.width().value / 72.0) * DEFAULT_PDF_IMAGE_DPI).round() as i32;
            let height = ((page.height().value / 72.0) * DEFAULT_PDF_IMAGE_DPI).round() as i32;
            let image = page
                .render_with_config(
                    &PdfRenderConfig::new()
                        .set_target_width(width.max(1))
                        .set_target_height(height.max(1)),
                )
                .map_err(|error| ApiError::Internal(error.to_string()))?
                .as_image();
            images.push(image);
        }
    }
    Ok(images)
}

fn bind_pdfium() -> ApiResult<Pdfium> {
    pdfium_auto::bind_bundled().map_err(|error| {
        ApiError::Internal(format!(
            "Failed to bind bundled PDFium library: {error}. Rebuild the project so pdfium-auto can install the platform PDFium binary."
        ))
    })
}

fn prepare_layout_image(image: &DynamicImage) -> ApiResult<DynamicImage> {
    let resized = image.resize_exact(
        LAYOUT_IMAGE_SIZE,
        LAYOUT_IMAGE_SIZE,
        imageops::FilterType::CatmullRom,
    );
    Ok(resized)
}

fn crop_block_image(image: &DynamicImage, block: &ContentBlock) -> ApiResult<DynamicImage> {
    let (width, height) = image.dimensions();
    let x1 = (block.bbox[0] * width as f32).floor().max(0.0) as u32;
    let y1 = (block.bbox[1] * height as f32).floor().max(0.0) as u32;
    let x2 = (block.bbox[2] * width as f32).ceil().min(width as f32) as u32;
    let y2 = (block.bbox[3] * height as f32).ceil().min(height as f32) as u32;
    if x2 <= x1 || y2 <= y1 {
        return Err(ApiError::BadRequest("Invalid block crop bbox".to_string()));
    }
    let cropped = image.crop_imm(x1, y1, x2 - x1, y2 - y1);
    let rotated = match block.angle {
        Some(90) => DynamicImage::ImageRgba8(imageops::rotate90(&cropped.to_rgba8())),
        Some(180) => DynamicImage::ImageRgba8(imageops::rotate180(&cropped.to_rgba8())),
        Some(270) => DynamicImage::ImageRgba8(imageops::rotate270(&cropped.to_rgba8())),
        _ => cropped,
    };
    Ok(rotated)
}

fn resize_by_need(image: DynamicImage) -> DynamicImage {
    let (width, height) = image.dimensions();
    let min_edge = width.min(height).max(1);
    let max_edge = width.max(height);
    let mut prepared = image;
    if max_edge as f32 / min_edge as f32 > MAX_IMAGE_EDGE_RATIO {
        let (new_width, new_height) = if width > height {
            (width, (width as f32 / MAX_IMAGE_EDGE_RATIO).ceil() as u32)
        } else {
            ((height as f32 / MAX_IMAGE_EDGE_RATIO).ceil() as u32, height)
        };
        let mut canvas =
            image::RgbaImage::from_pixel(new_width, new_height, Rgba([255, 255, 255, 255]));
        imageops::overlay(
            &mut canvas,
            &prepared.to_rgba8(),
            ((new_width - width) / 2) as i64,
            ((new_height - height) / 2) as i64,
        );
        prepared = DynamicImage::ImageRgba8(canvas);
    }
    let min_edge = prepared.width().min(prepared.height()).max(1);
    if min_edge < MIN_IMAGE_EDGE {
        let scale = MIN_IMAGE_EDGE as f32 / min_edge as f32;
        prepared = prepared.resize(
            (prepared.width() as f32 * scale).ceil() as u32,
            (prepared.height() as f32 * scale).ceil() as u32,
            imageops::FilterType::CatmullRom,
        );
    }
    prepared
}

fn encode_png(image: &DynamicImage) -> ApiResult<Vec<u8>> {
    let mut bytes = Cursor::new(Vec::new());
    image
        .write_to(&mut bytes, ImageFormat::Png)
        .map_err(|error| ApiError::Internal(error.to_string()))?;
    Ok(bytes.into_inner())
}

fn convert_bbox(values: [u32; 4]) -> Option<[f32; 4]> {
    if values.iter().any(|value| *value > 1000) {
        return None;
    }
    let (mut x1, mut y1, mut x2, mut y2) = (values[0], values[1], values[2], values[3]);
    if x2 < x1 {
        std::mem::swap(&mut x1, &mut x2);
    }
    if y2 < y1 {
        std::mem::swap(&mut y1, &mut y2);
    }
    if x1 == x2 || y1 == y2 {
        return None;
    }
    Some([
        x1 as f32 / 1000.0,
        y1 as f32 / 1000.0,
        x2 as f32 / 1000.0,
        y2 as f32 / 1000.0,
    ])
}

fn parse_angle(token: &str) -> Option<u16> {
    match token {
        "<|rotate_up|>" => Some(0),
        "<|rotate_right|>" => Some(90),
        "<|rotate_down|>" => Some(180),
        "<|rotate_left|>" => Some(270),
        _ => None,
    }
}

fn page_priority(page_index: usize) -> Option<i32> {
    Some(i32::try_from(page_index).unwrap_or(i32::MAX))
}

fn allowed_block_types() -> &'static HashSet<&'static str> {
    static TYPES: OnceLock<HashSet<&'static str>> = OnceLock::new();
    TYPES.get_or_init(|| {
        [
            "text",
            "title",
            "table",
            "equation",
            "code",
            "algorithm",
            "aside_text",
            "ref_text",
            "phonetic",
            "list_item",
            "table_caption",
            "image_caption",
            "code_caption",
            "table_footnote",
            "image_footnote",
            "header",
            "footer",
            "page_number",
            "page_footnote",
            "image",
            "chart",
            "list",
            "image_block",
            "equation_block",
            "unknown",
        ]
        .into_iter()
        .collect()
    })
}

fn skip_extract_types(image_analysis: bool) -> HashSet<&'static str> {
    let mut types = HashSet::from(["list", "equation_block", "image_block"]);
    if !image_analysis {
        types.insert("image");
        types.insert("chart");
    }
    types
}

fn blocks_to_markdown(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| block.content.as_deref())
        .filter(|content| !content.trim().is_empty())
        .collect::<Vec<&str>>()
        .join("\n\n")
}

#[derive(Debug, Clone)]
struct PageContent {
    content_list: Vec<Value>,
    content_list_v2: Vec<Value>,
    middle_para_blocks: Vec<Value>,
}

#[derive(Debug, Clone)]
struct ParaBlock {
    para_type: String,
    bbox: [f32; 4],
    index: usize,
    content: String,
    sub_type: Option<String>,
    children: Vec<ParaChild>,
}

#[derive(Debug, Clone)]
struct ParaChild {
    child_type: String,
    bbox: [f32; 4],
    index: usize,
    content: String,
}

/// Build MinerU Python-compatible content_list outputs from raw VLM layout blocks.
///
/// Inputs:
/// - `page_index`: zero-based page index.
/// - `blocks`: raw layout blocks after optional content extraction.
fn build_page_content(page_index: usize, blocks: &[ContentBlock]) -> PageContent {
    let para_blocks = build_para_blocks(blocks);
    PageContent {
        content_list: para_blocks
            .iter()
            .filter_map(|para| para_to_content_list_v1(page_index, para))
            .collect(),
        content_list_v2: para_blocks
            .iter()
            .filter_map(|para| para_to_content_list_v2(page_index, para))
            .collect(),
        middle_para_blocks: para_blocks.iter().map(para_to_middle_json).collect(),
    }
}

/// Convert flat raw layout blocks into a small Python MagicModel-like paragraph layer.
///
/// Inputs:
/// - `blocks`: page blocks in original layout order.
fn build_para_blocks(blocks: &[ContentBlock]) -> Vec<ParaBlock> {
    let child_parent_map = build_visual_child_parent_map(blocks);
    let mut para_blocks = Vec::new();

    for (index, block) in blocks.iter().enumerate() {
        if is_visual_child_type(&block.block_type) && child_parent_map[index].is_some() {
            continue;
        }

        let Some(mut para) = para_from_block(index, block) else {
            continue;
        };

        if is_visual_main_type(&para.para_type) {
            para.children = blocks
                .iter()
                .enumerate()
                .filter_map(|(child_index, child)| {
                    (child_parent_map[child_index] == Some(index))
                        .then(|| child_from_block(child_index, child, &para.para_type))
                        .flatten()
                })
                .collect();
        }

        para_blocks.push(para);
    }

    para_blocks.sort_by_key(|para| para.index);
    para_blocks
}

fn para_from_block(index: usize, block: &ContentBlock) -> Option<ParaBlock> {
    let content = block.content.clone().unwrap_or_default();
    let para_type = match block.block_type.as_str() {
        "equation" => "interline_equation",
        "algorithm" => "code",
        "list_item" => "text",
        "image_block" => "image",
        "image_caption" | "table_caption" | "code_caption" | "image_footnote"
        | "table_footnote" => "text",
        raw_type => raw_type,
    };
    let sub_type = match block.block_type.as_str() {
        "code" | "algorithm" => Some(block.block_type.clone()),
        "list" => Some("text".to_string()),
        _ => None,
    };

    if !is_visual_main_type(para_type) && content.trim().is_empty() {
        return None;
    }

    Some(ParaBlock {
        para_type: para_type.to_string(),
        bbox: block.bbox,
        index,
        content,
        sub_type,
        children: Vec::new(),
    })
}

fn child_from_block(index: usize, block: &ContentBlock, parent_type: &str) -> Option<ParaChild> {
    let content = block.content.clone().unwrap_or_default();
    if content.trim().is_empty() {
        return None;
    }
    let child_type = match (parent_type, block.block_type.as_str()) {
        ("image", "image_footnote" | "table_footnote") => "image_footnote",
        ("table", "image_footnote" | "table_footnote") => "table_footnote",
        ("chart", "image_footnote" | "table_footnote") => "chart_footnote",
        ("code", "image_footnote" | "table_footnote") => "code_footnote",
        ("image", _) => "image_caption",
        ("table", _) => "table_caption",
        ("chart", _) => "chart_caption",
        ("code", _) => "code_caption",
        _ => return None,
    };

    Some(ParaChild {
        child_type: child_type.to_string(),
        bbox: block.bbox,
        index,
        content,
    })
}

/// Associate raw caption/footnote blocks with the nearest visual/code parent.
///
/// Inputs:
/// - `blocks`: raw page blocks in layout order.
fn build_visual_child_parent_map(blocks: &[ContentBlock]) -> Vec<Option<usize>> {
    let main_indices = blocks
        .iter()
        .enumerate()
        .filter_map(|(index, block)| is_visual_main_raw_type(&block.block_type).then_some(index))
        .collect::<Vec<_>>();
    let mut parents = vec![None; blocks.len()];

    for (child_index, block) in blocks.iter().enumerate() {
        if !is_visual_child_type(&block.block_type) {
            continue;
        }
        let is_footnote = block.block_type.ends_with("_footnote");
        parents[child_index] = main_indices
            .iter()
            .copied()
            .filter(|parent_index| !is_footnote || *parent_index < child_index)
            .filter(|parent_index| visual_relation_is_clear(blocks, child_index, *parent_index))
            .min_by_key(|parent_index| parent_index.abs_diff(child_index));
    }

    parents
}

fn visual_relation_is_clear(
    blocks: &[ContentBlock],
    child_index: usize,
    parent_index: usize,
) -> bool {
    let start = child_index.min(parent_index) + 1;
    let end = child_index.max(parent_index);
    blocks[start..end].iter().all(|block| {
        is_visual_child_type(&block.block_type)
            || is_visual_relation_ignored_type(&block.block_type)
    })
}

fn para_to_content_list_v1(page_index: usize, para: &ParaBlock) -> Option<Value> {
    let mut value = match para.para_type.as_str() {
        "text" | "ref_text" | "phonetic" | "header" | "footer" | "page_number" | "aside_text"
        | "page_footnote" => json!({
            "type": para.para_type,
            "text": merge_text_v1(&para.content),
        }),
        "title" => json!({
            "type": "text",
            "text": merge_text_v1(&para.content),
            "text_level": 1,
        }),
        "interline_equation" => json!({
            "type": "equation",
            "text": clean_interline_equation(&para.content),
            "text_format": "latex",
        }),
        "image" => json!({
            "type": "image",
            "img_path": image_path_for_block(page_index, para.index),
            "image_caption": child_texts_v1(para, "image_caption"),
            "image_footnote": child_texts_v1(para, "image_footnote"),
            "content": para.content.trim(),
        }),
        "table" => {
            let mut table = json!({
                "type": "table",
                "img_path": image_path_for_block(page_index, para.index),
                "table_caption": child_texts_v1(para, "table_caption"),
                "table_footnote": child_texts_v1(para, "table_footnote"),
            });
            if !para.content.trim().is_empty() {
                table["table_body"] = json!(para.content.trim());
            }
            table
        }
        "chart" => json!({
            "type": "chart",
            "img_path": image_path_for_block(page_index, para.index),
            "chart_caption": child_texts_v1(para, "chart_caption"),
            "chart_footnote": child_texts_v1(para, "chart_footnote"),
            "content": para.content.trim(),
        }),
        "code" => {
            let sub_type = para.sub_type.as_deref().unwrap_or("code");
            let body = if sub_type == "code" {
                format!("```txt\n{}\n```", para.content.trim())
            } else {
                para.content.trim().to_string()
            };
            json!({
                "type": "code",
                "sub_type": sub_type,
                "code_caption": child_texts_v1(para, "code_caption"),
                "code_body": body,
            })
        }
        "list" => json!({
            "type": "list",
            "sub_type": para.sub_type.as_deref().unwrap_or("text"),
            "list_items": text_lines(&para.content),
        }),
        _ => return None,
    };

    value["bbox"] = json!(bbox_to_content_list(para.bbox));
    value["page_idx"] = json!(page_index);
    Some(value)
}

fn para_to_content_list_v2(page_index: usize, para: &ParaBlock) -> Option<Value> {
    let mut value = match para.para_type.as_str() {
        "header" | "footer" | "aside_text" | "page_number" | "page_footnote" => {
            let content_type = match para.para_type.as_str() {
                "header" => "page_header",
                "footer" => "page_footer",
                "aside_text" => "page_aside_text",
                "page_number" => "page_number",
                "page_footnote" => "page_footnote",
                _ => return None,
            };
            let mut content = serde_json::Map::new();
            content.insert(
                format!("{content_type}_content"),
                Value::Array(spans_v2(&para.content)),
            );
            json!({
                "type": content_type,
                "content": content
            })
        }
        "title" => json!({
            "type": "title",
            "content": {
                "title_content": spans_v2(&para.content),
                "level": 1,
            }
        }),
        "text" | "phonetic" => json!({
            "type": "paragraph",
            "content": {
                "paragraph_content": spans_v2(&para.content),
            }
        }),
        "ref_text" => json!({
            "type": "list",
            "content": {
                "list_type": "reference_list",
                "list_items": [{
                    "item_type": "text",
                    "item_content": spans_v2(&para.content),
                }],
            }
        }),
        "interline_equation" => json!({
            "type": "equation_interline",
            "content": {
                "math_content": clean_interline_equation(&para.content),
                "math_type": "latex",
                "image_source": {"path": image_path_for_block(page_index, para.index)},
            }
        }),
        "image" => json!({
            "type": "image",
            "content": {
                "image_source": {"path": image_path_for_block(page_index, para.index)},
                "content": para.content.trim(),
                "image_caption": child_spans_v2(para, "image_caption"),
                "image_footnote": child_spans_v2(para, "image_footnote"),
            }
        }),
        "table" => json!({
            "type": "table",
            "content": {
                "image_source": {"path": image_path_for_block(page_index, para.index)},
                "table_caption": child_spans_v2(para, "table_caption"),
                "table_footnote": child_spans_v2(para, "table_footnote"),
                "html": para.content.trim(),
                "table_type": table_type(&para.content),
                "table_nest_level": table_nest_level(&para.content),
            }
        }),
        "chart" => json!({
            "type": "chart",
            "content": {
                "image_source": {"path": image_path_for_block(page_index, para.index)},
                "content": para.content.trim(),
                "chart_caption": child_spans_v2(para, "chart_caption"),
                "chart_footnote": child_spans_v2(para, "chart_footnote"),
            }
        }),
        "code" => {
            let sub_type = para.sub_type.as_deref().unwrap_or("code");
            let content_type = if sub_type == "algorithm" {
                "algorithm"
            } else {
                "code"
            };
            if content_type == "algorithm" {
                json!({
                    "type": content_type,
                    "content": {
                        "algorithm_caption": child_spans_v2(para, "code_caption"),
                        "algorithm_content": spans_v2(&para.content),
                    }
                })
            } else {
                json!({
                    "type": content_type,
                    "content": {
                        "code_caption": child_spans_v2(para, "code_caption"),
                        "code_content": spans_v2(&para.content),
                        "code_language": "txt",
                    }
                })
            }
        }
        "list" => json!({
            "type": "list",
            "content": {
                "list_type": "text_list",
                "list_items": text_lines(&para.content)
                    .into_iter()
                    .map(|item| json!({
                        "item_type": "text",
                        "item_content": spans_v2(&item),
                    }))
                    .collect::<Vec<Value>>(),
            }
        }),
        _ => return None,
    };

    value["bbox"] = json!(bbox_to_content_list(para.bbox));
    Some(value)
}

fn para_to_middle_json(para: &ParaBlock) -> Value {
    json!({
        "type": para.para_type,
        "bbox": para.bbox,
        "index": para.index,
        "content": para.content,
        "sub_type": para.sub_type,
        "blocks": para.children.iter().map(|child| json!({
            "type": child.child_type,
            "bbox": child.bbox,
            "index": child.index,
            "content": child.content,
        })).collect::<Vec<Value>>(),
    })
}

fn child_texts_v1(para: &ParaBlock, child_type: &str) -> Vec<String> {
    para.children
        .iter()
        .filter(|child| child.child_type == child_type)
        .map(|child| merge_text_v1(&child.content))
        .collect()
}

fn child_spans_v2(para: &ParaBlock, child_type: &str) -> Vec<Value> {
    para.children
        .iter()
        .filter(|child| child.child_type == child_type)
        .flat_map(|child| spans_v2(&child.content))
        .collect()
}

fn spans_v2(content: &str) -> Vec<Value> {
    let content = merge_text_v2(content);
    if content.trim().is_empty() {
        Vec::new()
    } else {
        vec![json!({
            "type": "text",
            "content": content,
        })]
    }
}

fn text_lines(content: &str) -> Vec<String> {
    content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn merge_text_v1(content: &str) -> String {
    let content = normalize_text(content);
    if content.is_empty() {
        return String::new();
    }
    if contains_cjk(&content) {
        content
    } else if content.ends_with('-') {
        content
    } else {
        format!("{content} ")
    }
}

fn merge_text_v2(content: &str) -> String {
    merge_text_v1(content)
}

fn normalize_text(content: &str) -> String {
    content
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<&str>>()
        .join(" ")
}

fn clean_interline_equation(content: &str) -> String {
    let content = normalize_text(content);
    content
        .strip_prefix("\\[")
        .unwrap_or(&content)
        .strip_suffix("\\]")
        .unwrap_or_else(|| content.as_str())
        .trim()
        .to_string()
}

fn contains_cjk(content: &str) -> bool {
    content.chars().any(|ch| {
        matches!(
            ch as u32,
            0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0x3040..=0x30FF | 0xAC00..=0xD7AF
        )
    })
}

fn bbox_to_content_list(bbox: [f32; 4]) -> [i64; 4] {
    bbox.map(|coord| ((coord.clamp(0.0, 1.0) as f64) * 1000.0).floor() as i64)
}

fn image_path_for_block(page_index: usize, block_index: usize) -> String {
    format!("images/page_{page_index}_block_{block_index}.png")
}

fn table_nest_level(content: &str) -> usize {
    if content.matches("<table").count() > 1 {
        2
    } else {
        1
    }
}

fn table_type(content: &str) -> &'static str {
    if content.contains("colspan") || content.contains("rowspan") || table_nest_level(content) > 1 {
        "complex_table"
    } else {
        "simple_table"
    }
}

fn is_visual_main_raw_type(block_type: &str) -> bool {
    matches!(
        block_type,
        "image" | "image_block" | "table" | "chart" | "code" | "algorithm"
    )
}

fn is_visual_main_type(block_type: &str) -> bool {
    matches!(block_type, "image" | "table" | "chart" | "code")
}

fn is_visual_child_type(block_type: &str) -> bool {
    matches!(
        block_type,
        "image_caption" | "table_caption" | "code_caption" | "image_footnote" | "table_footnote"
    )
}

fn is_visual_relation_ignored_type(block_type: &str) -> bool {
    matches!(
        block_type,
        "header" | "footer" | "page_number" | "page_footnote" | "aside_text"
    )
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use crate::domain::models::ContentBlock;

    use super::{bind_pdfium, build_page_content, parse_layout_output};

    #[test]
    fn binds_bundled_pdfium() {
        bind_pdfium().expect("bundled PDFium should be available after build");
    }

    #[test]
    fn parses_layout_blocks() {
        let blocks = parse_layout_output(
            "<|box_start|>0 10 1000 200<|box_end|><|ref_start|>text<|ref_end|><|rotate_up|>",
        );
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].block_type, "text");
        assert_eq!(blocks[0].bbox, [0.0, 0.01, 1.0, 0.2]);
    }

    #[test]
    fn skips_inline_formula() {
        let blocks = parse_layout_output(
            "<|box_start|>0 0 100 100<|box_end|><|ref_start|>inline_formula<|ref_end|>",
        );
        assert!(blocks.is_empty());
    }

    #[test]
    fn content_list_maps_title_like_python() {
        let page = build_page_content(
            0,
            &[block(
                "title",
                [0.34200001, 0.12399999, 0.65399998, 0.14499999],
                "Attention Is All You Need",
            )],
        );

        assert_eq!(
            page.content_list[0],
            json!({
                "type": "text",
                "text": "Attention Is All You Need ",
                "text_level": 1,
                "bbox": [342, 123, 653, 144],
                "page_idx": 0
            })
        );
    }

    #[test]
    fn content_list_nests_visual_caption_instead_of_emitting_raw_type() {
        let page = build_page_content(
            0,
            &[
                block("image_caption", [0.1, 0.1, 0.9, 0.2], "Figure 1"),
                block("image", [0.1, 0.2, 0.9, 0.7], "A diagram"),
            ],
        );

        let top_level_types = page
            .content_list
            .iter()
            .filter_map(|value| value.get("type").and_then(Value::as_str))
            .collect::<Vec<_>>();
        assert_eq!(top_level_types, vec!["image"]);
        assert_eq!(page.content_list[0]["image_caption"], json!(["Figure 1 "]));
    }

    #[test]
    fn orphan_visual_caption_falls_back_to_text() {
        let page = build_page_content(
            0,
            &[block(
                "image_caption",
                [0.1, 0.1, 0.9, 0.2],
                "Unmatched caption",
            )],
        );

        assert_eq!(page.content_list[0]["type"], "text");
        assert_eq!(page.content_list[0]["text"], "Unmatched caption ");
    }

    #[test]
    fn content_list_maps_equation_like_python() {
        let page = build_page_content(
            0,
            &[block("equation", [0.0, 0.2, 1.0, 0.3], "\\[E=mc^2\\]")],
        );

        assert_eq!(
            page.content_list[0],
            json!({
                "type": "equation",
                "text": "E=mc^2",
                "text_format": "latex",
                "bbox": [0, 200, 1000, 300],
                "page_idx": 0
            })
        );
    }

    #[test]
    fn content_list_v2_uses_python_v2_shape() {
        let page = build_page_content(0, &[block("title", [0.0, 0.0, 1.0, 0.1], "Intro")]);

        assert_eq!(page.content_list[0]["type"], "text");
        assert_eq!(page.content_list_v2[0]["type"], "title");
        assert_eq!(
            page.content_list_v2[0]["content"]["title_content"],
            json!([{"type": "text", "content": "Intro "}])
        );
    }

    fn block(block_type: &str, bbox: [f32; 4], content: &str) -> ContentBlock {
        ContentBlock {
            block_type: block_type.to_string(),
            bbox,
            angle: None,
            content: Some(content.to_string()),
            merge_prev: None,
        }
    }
}
