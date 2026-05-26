use std::{
    collections::HashSet,
    io::Cursor,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

use futures::{stream, StreamExt};
use image::{imageops, DynamicImage, GenericImageView, ImageFormat, Rgba};
use pdfium_render::prelude::*;
use tokio::{fs, sync::Semaphore};

use crate::{
    domain::models::{ContentBlock, ParseTask, ParsedDocument, StoredUpload},
    error::{ApiError, ApiResult},
};

use super::client::{
    layout_prompt, layout_sampling_params, prompt_for_block, sampling_params_for_block,
    VlmHttpClient, VlmRequest,
};
use super::python_compat::{build_document_output, PythonPageInput};

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
    point_width: u32,
    point_height: u32,
    page_image: DynamicImage,
    blocks: Vec<ContentBlock>,
}

struct RenderedPage {
    page_index: usize,
    image: DynamicImage,
    point_width: u32,
    point_height: u32,
}

struct PageLayoutResult {
    page_index: usize,
    page_width: u32,
    page_height: u32,
    point_width: u32,
    point_height: u32,
    page_image: DynamicImage,
    blocks: Vec<ContentBlock>,
}

struct BlockExtractJob {
    page_index: usize,
    block_index: usize,
    block_type: String,
    image_png: Vec<u8>,
    store_image: bool,
}

struct BlockExtractResult {
    page_index: usize,
    block_index: usize,
    content: String,
    image_png: Option<Vec<u8>>,
}

struct RenderedPageWindow {
    pages: Vec<RenderedPage>,
    next_page_id: usize,
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
        let limiter = Arc::new(Semaphore::new(self.vlm_max_concurrency));
        let mut page_results = Vec::new();

        if upload.suffix == "pdf" {
            let bytes = fs::read(&upload.path).await?;
            let mut next_page_id = task.start_page_id;
            loop {
                let window = self
                    .load_pdf_page_window(&bytes, next_page_id, task.end_page_id)
                    .await?;
                if window.pages.is_empty() {
                    break;
                }
                next_page_id = window.next_page_id;
                page_results.extend(
                    self.parse_page_window(task, window.pages, limiter.clone())
                        .await?,
                );
                if next_page_id > task.end_page_id {
                    break;
                }
            }
        } else {
            let pages = self.load_image_pages(upload).await?;
            page_results.extend(self.parse_page_window(task, pages, limiter.clone()).await?);
        }

        page_results.sort_by_key(|result| result.page_index);
        let python_pages = page_results
            .into_iter()
            .map(|result| PythonPageInput {
                page_index: result.page_index,
                page_width: result.page_width,
                page_height: result.page_height,
                point_width: result.point_width,
                point_height: result.point_height,
                image: result.page_image,
                blocks: result.blocks,
            })
            .collect::<Vec<_>>();
        let output =
            build_document_output(&python_pages, &task.output_dir.join("_pending_images")).await?;

        Ok(ParsedDocument {
            file_name: upload.stem.clone(),
            markdown: output.markdown,
            middle_json: output.middle_json,
            model_output: output.model_output,
            content_list: output.content_list,
            content_list_v2: output.content_list_v2,
            image_files: output.image_files,
        })
    }

    /// Parse one rendered page window as layout, block preparation, and block extraction stages.
    ///
    /// Inputs:
    /// - `task`: request options and output directory.
    /// - `pages`: rendered page images in one processing window.
    /// - `limiter`: shared per-document VLM request limiter.
    async fn parse_page_window(
        &self,
        task: &ParseTask,
        pages: Vec<RenderedPage>,
        limiter: Arc<Semaphore>,
    ) -> ApiResult<Vec<PageParseResult>> {
        let unordered_layouts = stream::iter(
            pages
                .into_iter()
                .map(|page| self.detect_page_layout(task, page, limiter.clone())),
        )
        .buffer_unordered(self.vlm_max_concurrency)
        .collect::<Vec<_>>()
        .await;
        let mut layouts = unordered_layouts
            .into_iter()
            .collect::<ApiResult<Vec<PageLayoutResult>>>()?;
        layouts.sort_by_key(|layout| layout.page_index);

        let mut jobs = Vec::new();
        for layout in &layouts {
            jobs.extend(prepare_block_extract_jobs(task, layout)?);
        }

        let unordered_extracts = stream::iter(jobs.into_iter().map(|job| {
            self.extract_block(task, page_priority(job.page_index), job, limiter.clone())
        }))
        .buffer_unordered(self.vlm_max_concurrency)
        .collect::<Vec<_>>()
        .await;
        let mut extracts = unordered_extracts
            .into_iter()
            .collect::<ApiResult<Vec<BlockExtractResult>>>()?;
        extracts.sort_by_key(|result| (result.page_index, result.block_index));

        for result in extracts {
            if let Some(layout) = layouts
                .iter_mut()
                .find(|layout| layout.page_index == result.page_index)
            {
                if !result.content.is_empty() {
                    layout.blocks[result.block_index].content = Some(result.content);
                }
                if let Some(image_png) = result.image_png {
                    self.write_result_image_bytes(
                        task,
                        result.page_index,
                        result.block_index,
                        &image_png,
                    )
                    .await?;
                }
            }
        }

        Ok(layouts
            .into_iter()
            .map(|layout| PageParseResult {
                page_index: layout.page_index,
                page_width: layout.page_width,
                page_height: layout.page_height,
                point_width: layout.point_width,
                point_height: layout.point_height,
                page_image: layout.page_image,
                blocks: layout.blocks,
            })
            .collect())
    }

    /// Run MinerU layout detection for one rendered page.
    ///
    /// Inputs:
    /// - `task`: request options and output directory.
    /// - `page`: rendered PDF page or uploaded image.
    /// - `limiter`: shared per-document VLM request limiter.
    async fn detect_page_layout(
        &self,
        task: &ParseTask,
        page: RenderedPage,
        limiter: Arc<Semaphore>,
    ) -> ApiResult<PageLayoutResult> {
        let page_index = page.page_index;
        let page_image = page.image;
        let layout_image = encode_png(&prepare_layout_image(&page_image)?)?;
        let layout_output = self
            .predict_with_limit(
                &limiter,
                VlmRequest {
                    server_url: task.server_url.clone(),
                    prompt: layout_prompt().to_string(),
                    image_png: Some(layout_image),
                    sampling_params: layout_sampling_params(),
                    priority: page_priority(page_index),
                },
            )
            .await?;
        let blocks = parse_layout_output(&layout_output);

        Ok(PageLayoutResult {
            page_index,
            page_width: page_image.width(),
            page_height: page_image.height(),
            point_width: page.point_width,
            point_height: page.point_height,
            page_image,
            blocks,
        })
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
            page_index: job.page_index,
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

    async fn load_image_pages(&self, upload: &StoredUpload) -> ApiResult<Vec<RenderedPage>> {
        let bytes = fs::read(&upload.path).await?;
        let image = image::load_from_memory(&bytes)
            .map_err(|error| ApiError::BadRequest(format!("Failed to load image: {error}")))?;
        Ok(vec![RenderedPage {
            page_index: 0,
            point_width: image.width(),
            point_height: image.height(),
            image,
        }])
    }

    async fn load_pdf_page_window(
        &self,
        bytes: &[u8],
        start_page_id: usize,
        end_page_id: usize,
    ) -> ApiResult<RenderedPageWindow> {
        let bytes = bytes.to_vec();
        let window_size = self.processing_window_size.max(1);
        tokio::task::spawn_blocking(move || {
            render_pdf_page_window(&bytes, start_page_id, end_page_id, window_size)
        })
        .await
        .map_err(|error| ApiError::Internal(error.to_string()))?
    }
}

/// Prepare all block extraction jobs for one page after layout detection.
///
/// Inputs:
/// - `task`: request options controlling skipped block types.
/// - `layout`: page layout result with source page image and detected blocks.
fn prepare_block_extract_jobs(
    task: &ParseTask,
    layout: &PageLayoutResult,
) -> ApiResult<Vec<BlockExtractJob>> {
    let skip_types = skip_extract_types(task.image_analysis);
    let mut jobs = Vec::new();
    for (block_index, block) in layout.blocks.iter().enumerate() {
        if skip_types.contains(block.block_type.as_str()) {
            continue;
        }
        let block_image = crop_block_image(&layout.page_image, block)?;
        if block_image.width() < 1 || block_image.height() < 1 {
            continue;
        }
        let block_image = resize_by_need(block_image);
        jobs.push(BlockExtractJob {
            page_index: layout.page_index,
            block_index,
            block_type: block.block_type.clone(),
            image_png: encode_png(&block_image)?,
            store_image: false,
        });
    }
    Ok(jobs)
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

fn render_pdf_page_window(
    bytes: &[u8],
    start_page_id: usize,
    end_page_id: usize,
    processing_window_size: usize,
) -> ApiResult<RenderedPageWindow> {
    let pdfium = bind_pdfium()?;
    let document = pdfium
        .load_pdf_from_byte_slice(bytes, None)
        .map_err(|error| ApiError::BadRequest(format!("Failed to open PDF: {error}")))?;
    let page_count = document.pages().len() as usize;
    if page_count == 0 {
        return Ok(RenderedPageWindow {
            pages: Vec::new(),
            next_page_id: end_page_id.saturating_add(1),
        });
    }
    let start = start_page_id.min(page_count - 1);
    let end = end_page_id.min(page_count - 1);
    if start > end {
        return Ok(RenderedPageWindow {
            pages: Vec::new(),
            next_page_id: end.saturating_add(1),
        });
    }
    let window_end = end.min(start + processing_window_size - 1);
    let mut images = Vec::new();
    for page_index in start..=window_end {
        let page = document
            .pages()
            .get(page_index as u16)
            .map_err(|error| ApiError::Internal(error.to_string()))?;
        let point_width = page.width().value.round().max(1.0) as u32;
        let point_height = page.height().value.round().max(1.0) as u32;
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
        images.push(RenderedPage {
            page_index,
            image,
            point_width,
            point_height,
        });
    }
    Ok(RenderedPageWindow {
        pages: images,
        next_page_id: window_end.saturating_add(1),
    })
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

#[cfg(test)]
mod tests {
    use std::{
        env,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        time::Duration,
    };

    use axum::{extract::State, http::StatusCode, routing::get, routing::post, Json, Router};
    use chrono::Utc;
    use image::{DynamicImage, ImageBuffer, ImageFormat, Rgb};
    use serde_json::{json, Value};
    use tempfile::tempdir;
    use tokio::{net::TcpListener, sync::oneshot, time::sleep};
    use uuid::Uuid;

    use crate::domain::models::{ParseOptions, ParseTask, StoredUpload, TaskStatus};
    use crate::vlm::client::VlmHttpClient;

    use super::{bind_pdfium, parse_layout_output, VlmDocumentParser};

    static TEST_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[derive(Clone)]
    struct TestVlmState {
        models_count: Arc<AtomicUsize>,
        chat_count: Arc<AtomicUsize>,
        active_layouts: Arc<AtomicUsize>,
        max_active_layouts: Arc<AtomicUsize>,
        fail_chat: bool,
    }

    struct EnvVarGuard {
        name: &'static str,
        previous: Option<String>,
    }

    impl EnvVarGuard {
        fn set(name: &'static str, value: &str) -> Self {
            let previous = env::var(name).ok();
            env::set_var(name, value);
            Self { name, previous }
        }

        fn unset(name: &'static str) -> Self {
            let previous = env::var(name).ok();
            env::remove_var(name);
            Self { name, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(value) = &self.previous {
                env::set_var(self.name, value);
            } else {
                env::remove_var(self.name);
            }
        }
    }

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

    #[tokio::test]
    async fn parser_uses_window_layout_then_block_extraction_with_ordered_output() {
        let _env_lock = TEST_ENV_LOCK.lock().await;
        let _no_proxy = EnvVarGuard::set("NO_PROXY", "127.0.0.1,localhost");
        let (base_url, state, server) = spawn_test_vlm_server(false).await;
        let _server_url = EnvVarGuard::set("MINERU_VL_SERVER", &base_url);
        let _model_name = EnvVarGuard::unset("MINERU_VL_MODEL_NAME");
        let temp = tempdir().expect("temp dir");
        let upload_path = temp.path().join("sample.png");
        ImageBuffer::from_pixel(8, 8, Rgb([255_u8, 255, 255]))
            .save_with_format(&upload_path, ImageFormat::Png)
            .expect("png fixture");
        let task = completed_test_task(temp.path().to_path_buf(), upload_path);
        let parser = VlmDocumentParser::new(Arc::new(VlmHttpClient::new()), 4, 4);

        let file_names = parser.parse_task(&task).await.expect("parse succeeds");

        server.abort();
        assert_eq!(file_names, vec!["sample"]);
        assert_eq!(state.models_count.load(Ordering::SeqCst), 1);
        assert_eq!(state.chat_count.load(Ordering::SeqCst), 2);
        assert!(state.max_active_layouts.load(Ordering::SeqCst) >= 1);
        let markdown =
            tokio::fs::read_to_string(temp.path().join("sample").join("vlm").join("sample.md"))
                .await
                .expect("markdown should be written");
        assert!(markdown.contains("recognized text"));
    }

    #[tokio::test]
    async fn page_window_runs_layouts_before_window_block_extraction() {
        let _env_lock = TEST_ENV_LOCK.lock().await;
        let _no_proxy = EnvVarGuard::set("NO_PROXY", "127.0.0.1,localhost");
        let (base_url, state, server) = spawn_test_vlm_server(false).await;
        let _server_url = EnvVarGuard::set("MINERU_VL_SERVER", &base_url);
        let _model_name = EnvVarGuard::unset("MINERU_VL_MODEL_NAME");
        let temp = tempdir().expect("temp dir");
        let task = ParseTask::new(
            Uuid::new_v4(),
            &ParseOptions::default(),
            vec![StoredUpload {
                stem: "sample".to_string(),
                path: temp.path().join("sample.png"),
                suffix: "png".to_string(),
            }],
            temp.path().to_path_buf(),
        );
        let parser = VlmDocumentParser::new(Arc::new(VlmHttpClient::new()), 2, 2);
        let pages = vec![
            rendered_test_page(1, [255, 0, 0]),
            rendered_test_page(0, [0, 255, 0]),
        ];

        let results = parser
            .parse_page_window(&task, pages, Arc::new(tokio::sync::Semaphore::new(2)))
            .await
            .expect("window parse succeeds");

        server.abort();
        assert_eq!(
            results
                .iter()
                .map(|result| result.page_index)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert_eq!(state.models_count.load(Ordering::SeqCst), 1);
        assert_eq!(state.chat_count.load(Ordering::SeqCst), 4);
        assert_eq!(state.max_active_layouts.load(Ordering::SeqCst), 2);
        assert!(results
            .iter()
            .all(|result| result.blocks[0].content.as_deref() == Some("recognized text")));
    }

    #[tokio::test]
    async fn parser_surfaces_chat_completion_failures() {
        let _env_lock = TEST_ENV_LOCK.lock().await;
        let _no_proxy = EnvVarGuard::set("NO_PROXY", "127.0.0.1,localhost");
        let (base_url, _state, server) = spawn_test_vlm_server(true).await;
        let _server_url = EnvVarGuard::set("MINERU_VL_SERVER", &base_url);
        let _model_name = EnvVarGuard::unset("MINERU_VL_MODEL_NAME");
        let temp = tempdir().expect("temp dir");
        let upload_path = temp.path().join("sample.png");
        ImageBuffer::from_pixel(8, 8, Rgb([255_u8, 255, 255]))
            .save_with_format(&upload_path, ImageFormat::Png)
            .expect("png fixture");
        let task = completed_test_task(temp.path().to_path_buf(), upload_path);
        let parser = VlmDocumentParser::new(Arc::new(VlmHttpClient::new()), 4, 4);

        let error = parser.parse_task(&task).await.expect_err("parse fails");

        server.abort();
        assert!(error.detail().contains("500 Internal Server Error"));
    }

    async fn spawn_test_vlm_server(
        fail_chat: bool,
    ) -> (String, TestVlmState, tokio::task::JoinHandle<()>) {
        let state = TestVlmState {
            models_count: Arc::new(AtomicUsize::new(0)),
            chat_count: Arc::new(AtomicUsize::new(0)),
            active_layouts: Arc::new(AtomicUsize::new(0)),
            max_active_layouts: Arc::new(AtomicUsize::new(0)),
            fail_chat,
        };
        let app = Router::new()
            .route("/v1/models", post(test_models).get(test_models))
            .route("/v1/chat/completions", post(test_chat_completions))
            .route("/ready", get(|| async { "ok" }))
            .with_state(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test server must bind");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        let (ready_sender, ready_receiver) = oneshot::channel();
        let server = tokio::spawn(async move {
            let _ = ready_sender.send(());
            axum::serve(listener, app)
                .await
                .expect("test server must run");
        });
        ready_receiver.await.expect("server should start");
        wait_until_ready(&base_url).await;
        (base_url, state, server)
    }

    async fn wait_until_ready(base_url: &str) {
        let ready_url = format!("{base_url}/ready");
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("ready client should build");
        for _ in 0..100 {
            if let Ok(response) = client.get(&ready_url).send().await {
                if response.status().is_success() {
                    return;
                }
            }
            sleep(Duration::from_millis(10)).await;
        }
        panic!("test server did not become ready");
    }

    async fn test_models(State(state): State<TestVlmState>) -> Json<Value> {
        state.models_count.fetch_add(1, Ordering::SeqCst);
        Json(json!({
            "object": "list",
            "data": [{ "id": "test-model", "object": "model" }]
        }))
    }

    async fn test_chat_completions(
        State(state): State<TestVlmState>,
        Json(payload): Json<Value>,
    ) -> Result<Json<Value>, StatusCode> {
        state.chat_count.fetch_add(1, Ordering::SeqCst);
        if state.fail_chat {
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
        let prompt = extract_prompt_text(&payload);
        if prompt.contains("Layout Detection") {
            let active = state.active_layouts.fetch_add(1, Ordering::SeqCst) + 1;
            state.max_active_layouts.fetch_max(active, Ordering::SeqCst);
            sleep(Duration::from_millis(20)).await;
            state.active_layouts.fetch_sub(1, Ordering::SeqCst);
            return Ok(Json(chat_payload(
                "<|box_start|>0 0 1000 1000<|box_end|><|ref_start|>text<|ref_end|>",
            )));
        }
        Ok(Json(chat_payload("recognized text")))
    }

    fn extract_prompt_text(payload: &Value) -> String {
        payload
            .get("messages")
            .and_then(Value::as_array)
            .and_then(|messages| messages.get(1))
            .and_then(|message| message.get("content"))
            .and_then(Value::as_array)
            .and_then(|content| content.last())
            .and_then(|item| item.get("text"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    fn chat_payload(content: &str) -> Value {
        json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "finish_reason": "stop",
                "message": { "role": "assistant", "content": content }
            }]
        })
    }

    fn rendered_test_page(page_index: usize, color: [u8; 3]) -> super::RenderedPage {
        let image = DynamicImage::ImageRgb8(ImageBuffer::from_pixel(
            8,
            8,
            Rgb([color[0], color[1], color[2]]),
        ));
        super::RenderedPage {
            page_index,
            point_width: image.width(),
            point_height: image.height(),
            image,
        }
    }

    fn completed_test_task(
        output_dir: std::path::PathBuf,
        upload_path: std::path::PathBuf,
    ) -> ParseTask {
        ParseTask {
            task_id: Uuid::new_v4(),
            status: TaskStatus::Completed,
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
            uploads: vec![upload_path],
            upload_suffixes: vec!["png".to_string()],
            submit_order: 0,
            started_at: None,
            completed_at: Some(Utc::now()),
            error: None,
        }
    }
}
