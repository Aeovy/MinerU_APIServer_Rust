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
        let mut image_files = Vec::new();

        for result in page_results {
            let page_index = result.page_index;
            let blocks = result.blocks;
            markdown_parts.push(blocks_to_markdown(&blocks));
            content_list.extend(blocks_to_content_list(page_index, &blocks));
            image_files.extend(result.image_files);
            all_page_blocks.push(json!({
                "preproc_blocks": blocks,
                "discarded_blocks": [],
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
            serde_json::to_vec_pretty(&document.content_list)?,
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

fn blocks_to_content_list(page_index: usize, blocks: &[ContentBlock]) -> Vec<Value> {
    blocks
        .iter()
        .filter_map(|block| {
            let content = block.content.as_ref()?;
            Some(json!({
                "type": block.block_type,
                "text": content,
                "page_idx": page_index,
                "bbox": block.bbox
            }))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{bind_pdfium, parse_layout_output};

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
}
