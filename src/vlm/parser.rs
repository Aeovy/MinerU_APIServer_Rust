use std::{
    collections::HashSet,
    io::Cursor,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::Instant,
};

use fast_image_resize::{
    images::Image as FastImage, pixels::PixelType, FilterType, ResizeAlg, ResizeOptions, Resizer,
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
    VlmHttpClient, VlmRequest, VlmSession,
};
use super::python_compat::{
    build_page_output_fragment, DocumentOutputAccumulator, PythonPageInput,
    PythonPageOutputFragment,
};

const DEFAULT_PDF_IMAGE_DPI: f32 = 200.0;
const LAYOUT_IMAGE_SIZE: u32 = 1036;
const MIN_IMAGE_EDGE: u32 = 28;
const MAX_IMAGE_EDGE_RATIO: f32 = 50.0;
const MAX_IMAGE_PREPROCESS_THREADS: usize = 64;

#[derive(Clone)]
pub struct VlmDocumentParser {
    client: Arc<VlmHttpClient>,
    processing_window_size: usize,
    vlm_max_concurrency: usize,
    vlm_semaphore: Arc<Semaphore>,
    image_preprocess_pool: Arc<rayon::ThreadPool>,
}

struct RenderedPage {
    page_index: usize,
    image: Arc<DynamicImage>,
    point_width: u32,
    point_height: u32,
}

struct PreparedLayoutPage {
    page: RenderedPage,
    image_png: Vec<u8>,
    prepare_ms: u128,
}

struct PageLayoutResult {
    page_index: usize,
    page_width: u32,
    page_height: u32,
    point_width: u32,
    point_height: u32,
    page_image: Arc<DynamicImage>,
    blocks: Vec<ContentBlock>,
}

struct PagePipelineResult {
    page: PythonPageOutputFragment,
    block_count: usize,
    extract_job_count: usize,
    layout_prepare_ms: u128,
    layout_vlm_ms: u128,
    prepare_blocks_ms: u128,
    extract_ms: u128,
    apply_results_ms: u128,
}

struct BlockExtractJob {
    page_index: usize,
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

struct RenderedPageWindow {
    pages: Vec<RenderedPage>,
    next_page_id: usize,
}

impl VlmDocumentParser {
    pub fn new(
        client: Arc<VlmHttpClient>,
        processing_window_size: usize,
        vlm_max_concurrency: usize,
        image_preprocess_threads: usize,
    ) -> ApiResult<Self> {
        let image_preprocess_pool = rayon::ThreadPoolBuilder::new()
            .num_threads(normalize_image_preprocess_threads(image_preprocess_threads))
            .thread_name(|index| format!("mineru-image-preprocess-{index}"))
            .build()
            .map_err(|error| {
                ApiError::Internal(format!(
                    "Failed to build image preprocess thread pool: {error}"
                ))
            })?;
        Ok(Self {
            client,
            processing_window_size,
            vlm_max_concurrency: vlm_max_concurrency.max(1),
            vlm_semaphore: Arc::new(Semaphore::new(vlm_max_concurrency.max(1))),
            image_preprocess_pool: Arc::new(image_preprocess_pool),
        })
    }

    pub fn available_vlm_permits(&self) -> usize {
        self.vlm_semaphore.available_permits()
    }

    /// Parse all uploads in a task and persist MinerU-compatible result files.
    ///
    /// Inputs:
    /// - `task`: task options and output directory.
    pub async fn parse_task(&self, task: &ParseTask) -> ApiResult<Vec<String>> {
        let started_at = Instant::now();
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
            let upload_started_at = Instant::now();
            let document = self.parse_upload(task, &upload).await?;
            let parse_upload_ms = upload_started_at.elapsed().as_millis();
            let write_started_at = Instant::now();
            self.write_document(&task.output_dir, &document).await?;
            tracing::debug!(
                task_id = %task.task_id,
                file_name = %document.file_name,
                suffix = %upload.suffix,
                parse_upload_ms,
                write_document_ms = write_started_at.elapsed().as_millis(),
                "document parse output written"
            );
            response_file_names.push(document.file_name);
        }
        tracing::debug!(
            task_id = %task.task_id,
            file_count = response_file_names.len(),
            elapsed_ms = started_at.elapsed().as_millis(),
            "parse task documents completed"
        );
        Ok(response_file_names)
    }

    async fn parse_upload(
        &self,
        task: &ParseTask,
        upload: &StoredUpload,
    ) -> ApiResult<ParsedDocument> {
        let started_at = Instant::now();
        let session_started_at = Instant::now();
        let session = self
            .client
            .session_for_request(task.server_url.as_deref())
            .await?;
        tracing::debug!(
            task_id = %task.task_id,
            file_name = %upload.stem,
            elapsed_ms = session_started_at.elapsed().as_millis(),
            "vlm session resolved"
        );
        let pending_image_dir = task.output_dir.join("_pending_images");
        let mut output_builder = DocumentOutputAccumulator::new();
        let mut page_count = 0_usize;
        tracing::debug!(
            task_id = %task.task_id,
            file_name = %upload.stem,
            suffix = %upload.suffix,
            processing_window_size = self.processing_window_size,
            vlm_max_concurrency = self.vlm_max_concurrency,
            start_page_id = task.start_page_id,
            end_page_id = task.end_page_id,
            "parse upload started"
        );

        if upload.suffix == "pdf" {
            let metadata_started_at = Instant::now();
            let pdf_path = upload.path.clone();
            let pdf_byte_len = fs::metadata(&pdf_path).await?.len();
            tracing::debug!(
                task_id = %task.task_id,
                file_name = %upload.stem,
                byte_len = pdf_byte_len,
                elapsed_ms = metadata_started_at.elapsed().as_millis(),
                "pdf file ready for windowed rendering"
            );
            let mut next_page_id = task.start_page_id;
            loop {
                let render_started_at = Instant::now();
                let requested_start_page = next_page_id;
                let window = self
                    .load_pdf_page_window(pdf_path.clone(), next_page_id, task.end_page_id)
                    .await?;
                let render_elapsed_ms = render_started_at.elapsed().as_millis();
                if window.pages.is_empty() {
                    tracing::debug!(
                        task_id = %task.task_id,
                        file_name = %upload.stem,
                        requested_start_page,
                        elapsed_ms = render_elapsed_ms,
                        "pdf page window empty"
                    );
                    break;
                }
                let window_page_count = window.pages.len();
                let window_first_page = window.pages.first().map(|page| page.page_index);
                let window_last_page = window.pages.last().map(|page| page.page_index);
                next_page_id = window.next_page_id;
                tracing::debug!(
                    task_id = %task.task_id,
                    file_name = %upload.stem,
                    first_page = window_first_page,
                    last_page = window_last_page,
                    page_count = window_page_count,
                    next_page_id,
                    elapsed_ms = render_elapsed_ms,
                    "pdf page window rendered"
                );
                let parse_window_started_at = Instant::now();
                let page_fragments = self
                    .parse_page_window(task, &session, window.pages, pending_image_dir.clone())
                    .await?;
                page_count += page_fragments.len();
                output_builder.append_fragments(page_fragments);
                tracing::debug!(
                    task_id = %task.task_id,
                    file_name = %upload.stem,
                    first_page = window_first_page,
                    last_page = window_last_page,
                    page_count = window_page_count,
                    elapsed_ms = parse_window_started_at.elapsed().as_millis(),
                    "pdf page window parsed"
                );
                if next_page_id > task.end_page_id {
                    break;
                }
            }
        } else {
            let load_started_at = Instant::now();
            let pages = self.load_image_pages(upload).await?;
            tracing::debug!(
                task_id = %task.task_id,
                file_name = %upload.stem,
                page_count = pages.len(),
                elapsed_ms = load_started_at.elapsed().as_millis(),
                "image pages loaded"
            );
            let parse_window_started_at = Instant::now();
            let page_fragments = self
                .parse_page_window(task, &session, pages, pending_image_dir.clone())
                .await?;
            page_count += page_fragments.len();
            output_builder.append_fragments(page_fragments);
            tracing::debug!(
                task_id = %task.task_id,
                file_name = %upload.stem,
                elapsed_ms = parse_window_started_at.elapsed().as_millis(),
                "image page window parsed"
            );
        }

        let output_started_at = Instant::now();
        let output = output_builder.finish();
        tracing::debug!(
            task_id = %task.task_id,
            file_name = %upload.stem,
            page_count,
            elapsed_ms = output_started_at.elapsed().as_millis(),
            "document output assembled"
        );

        let document = ParsedDocument {
            file_name: upload.stem.clone(),
            markdown: output.markdown,
            middle_json: output.middle_json,
            model_output: output.model_output,
            content_list: output.content_list,
            content_list_v2: output.content_list_v2,
            image_files: output.image_files,
        };
        tracing::debug!(
            task_id = %task.task_id,
            file_name = %document.file_name,
            page_count,
            elapsed_ms = started_at.elapsed().as_millis(),
            "parse upload completed"
        );
        Ok(document)
    }

    /// Parse one rendered page window with page-level pipelining.
    ///
    /// Inputs:
    /// - `task`: request options and output directory.
    /// - `pages`: rendered page images in one processing window.
    /// - `pending_image_dir`: temporary crop directory used by Python-compatible output.
    async fn parse_page_window(
        &self,
        task: &ParseTask,
        session: &VlmSession,
        pages: Vec<RenderedPage>,
        pending_image_dir: PathBuf,
    ) -> ApiResult<Vec<PythonPageOutputFragment>> {
        let started_at = Instant::now();
        let page_count = pages.len();
        let first_page = pages.first().map(|page| page.page_index);
        let last_page = pages.last().map(|page| page.page_index);
        tracing::debug!(
            task_id = %task.task_id,
            page_count,
            first_page,
            last_page,
            "page window parse started"
        );
        fs::create_dir_all(&pending_image_dir).await?;
        let limiter = self.vlm_semaphore.clone();
        let page_concurrency = page_count.min(self.vlm_max_concurrency).max(1);
        let mut pipeline_results = Vec::with_capacity(page_count);
        let mut pending = stream::iter(pages.into_iter().map(|page| {
            self.parse_one_page_pipeline(
                task,
                session,
                page,
                limiter.clone(),
                pending_image_dir.clone(),
            )
        }))
        .buffer_unordered(page_concurrency);

        while let Some(result) = pending.next().await {
            match result {
                Ok(page) => pipeline_results.push(page),
                Err(error) => {
                    tracing::debug!(
                        task_id = %task.task_id,
                        error = %error.detail(),
                        completed_page_count = pipeline_results.len(),
                        "page pipeline failed; cancelling remaining window pages"
                    );
                    return Err(error);
                }
            }
        }
        pipeline_results.sort_by_key(|result| result.page.page_index());
        let block_count = pipeline_results
            .iter()
            .map(|result| result.block_count)
            .sum::<usize>();
        let extract_job_count = pipeline_results
            .iter()
            .map(|result| result.extract_job_count)
            .sum::<usize>();
        let layout_prepare_ms = pipeline_results
            .iter()
            .map(|result| result.layout_prepare_ms)
            .sum::<u128>();
        let layout_vlm_ms = pipeline_results
            .iter()
            .map(|result| result.layout_vlm_ms)
            .sum::<u128>();
        let prepare_blocks_ms = pipeline_results
            .iter()
            .map(|result| result.prepare_blocks_ms)
            .sum::<u128>();
        let extract_ms = pipeline_results
            .iter()
            .map(|result| result.extract_ms)
            .sum::<u128>();
        let apply_results_ms = pipeline_results
            .iter()
            .map(|result| result.apply_results_ms)
            .sum::<u128>();

        let results = pipeline_results
            .into_iter()
            .map(|result| result.page)
            .collect::<Vec<_>>();
        tracing::debug!(
            task_id = %task.task_id,
            page_count = results.len(),
            first_page,
            last_page,
            block_count,
            extract_job_count,
            layout_prepare_ms,
            layout_vlm_ms,
            prepare_blocks_ms,
            extract_ms,
            apply_results_ms,
            elapsed_ms = started_at.elapsed().as_millis(),
            "page window parse completed"
        );
        Ok(results)
    }

    /// Parse one page through layout detection, block crop preparation, and block extraction.
    ///
    /// Inputs:
    /// - `task`: request options and output directory.
    /// - `page`: one rendered PDF page or uploaded image page.
    /// - `limiter`: global VLM request limiter shared by all documents.
    /// - `pending_image_dir`: temporary crop directory used by Python-compatible output.
    async fn parse_one_page_pipeline(
        &self,
        task: &ParseTask,
        session: &VlmSession,
        page: RenderedPage,
        limiter: Arc<Semaphore>,
        pending_image_dir: PathBuf,
    ) -> ApiResult<PagePipelineResult> {
        let started_at = Instant::now();
        let page_index = page.page_index;

        let prepared_page =
            prepare_page_layout_image_async(self.image_preprocess_pool.clone(), page).await?;
        let layout_prepare_ms = prepared_page.prepare_ms;
        let layout_started_at = Instant::now();
        let mut layout = self
            .detect_page_layout(task, session, prepared_page, limiter.clone())
            .await?;
        let layout_vlm_ms = layout_started_at.elapsed().as_millis();

        let prepare_blocks_started_at = Instant::now();
        let jobs = prepare_page_extract_jobs_async(
            self.image_preprocess_pool.clone(),
            task.image_analysis,
            &layout,
        )
        .await?;
        let block_count = layout.blocks.len();
        let extract_job_count = jobs.len();
        let prepare_blocks_ms = prepare_blocks_started_at.elapsed().as_millis();

        let extract_started_at = Instant::now();
        let mut extracts = self
            .extract_window_blocks(task, session, jobs, limiter.clone())
            .await?;
        extracts.sort_by_key(|result| result.block_index);
        let extract_ms = extract_started_at.elapsed().as_millis();

        let apply_started_at = Instant::now();
        for result in extracts {
            if !result.content.is_empty() {
                layout.blocks[result.block_index].content = Some(result.content);
            }
            if let Some(image_png) = result.image_png {
                self.write_result_image_bytes(task, page_index, result.block_index, &image_png)
                    .await?;
            }
        }
        let apply_results_ms = apply_started_at.elapsed().as_millis();
        let output_started_at = Instant::now();
        let page_fragment = build_page_output_fragment(
            PythonPageInput {
                page_index: layout.page_index,
                page_width: layout.page_width,
                page_height: layout.page_height,
                point_width: layout.point_width,
                point_height: layout.point_height,
                image: layout.page_image,
                blocks: layout.blocks,
            },
            &pending_image_dir,
        )
        .await?;
        let output_fragment_ms = output_started_at.elapsed().as_millis();

        tracing::debug!(
            task_id = %task.task_id,
            page_index,
            block_count,
            extract_job_count,
            layout_prepare_ms,
            layout_vlm_ms,
            prepare_blocks_ms,
            extract_ms,
            apply_results_ms,
            output_fragment_ms,
            elapsed_ms = started_at.elapsed().as_millis(),
            "page pipeline completed"
        );

        Ok(PagePipelineResult {
            page: page_fragment,
            block_count,
            extract_job_count,
            layout_prepare_ms,
            layout_vlm_ms,
            prepare_blocks_ms,
            extract_ms,
            apply_results_ms,
        })
    }

    /// Run MinerU layout detection for one rendered page.
    ///
    /// Inputs:
    /// - `task`: request options and output directory.
    /// - `page`: rendered PDF page or uploaded image.
    /// - `limiter`: global VLM request limiter shared by all documents.
    async fn detect_page_layout(
        &self,
        task: &ParseTask,
        session: &VlmSession,
        prepared_page: PreparedLayoutPage,
        limiter: Arc<Semaphore>,
    ) -> ApiResult<PageLayoutResult> {
        let started_at = Instant::now();
        let PreparedLayoutPage {
            page,
            image_png,
            prepare_ms,
        } = prepared_page;
        let page_index = page.page_index;
        let page_image = page.image;
        let layout_image_len = image_png.len();
        let predict_started_at = Instant::now();
        let layout_output = self
            .predict_with_limit(
                &limiter,
                session,
                VlmRequest {
                    prompt: layout_prompt().to_string(),
                    image_png: Some(image_png),
                    sampling_params: layout_sampling_params(),
                    priority: page_priority(page_index),
                },
            )
            .await?;
        let predict_elapsed_ms = predict_started_at.elapsed().as_millis();
        let blocks = parse_layout_output(&layout_output);
        tracing::debug!(
            task_id = %task.task_id,
            page_index,
            image_width = page_image.width(),
            image_height = page_image.height(),
            prepared_image_bytes = layout_image_len,
            block_count = blocks.len(),
            prepare_image_ms = prepare_ms,
            vlm_ms = predict_elapsed_ms,
            elapsed_ms = started_at.elapsed().as_millis(),
            "page layout detected"
        );

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
    /// - `limiter`: global VLM request limiter shared by all documents.
    async fn extract_block(
        &self,
        task: &ParseTask,
        session: &VlmSession,
        priority: Option<i32>,
        job: BlockExtractJob,
        limiter: Arc<Semaphore>,
    ) -> ApiResult<BlockExtractResult> {
        let started_at = Instant::now();
        let page_index = job.page_index;
        let block_index = job.block_index;
        let block_type = job.block_type.clone();
        let image_png = job.image_png;
        let image_len = image_png.len();
        let result_image_png = job.store_image.then(|| image_png.clone());
        let content = self
            .predict_with_limit(
                &limiter,
                session,
                VlmRequest {
                    prompt: prompt_for_block(&job.block_type).to_string(),
                    image_png: Some(image_png),
                    sampling_params: sampling_params_for_block(&job.block_type),
                    priority,
                },
            )
            .await?;

        tracing::debug!(
            task_id = %task.task_id,
            page_index,
            block_index,
            block_type = %block_type,
            image_bytes = image_len,
            content_chars = content.chars().count(),
            elapsed_ms = started_at.elapsed().as_millis(),
            "block extracted"
        );
        Ok(BlockExtractResult {
            block_index,
            content,
            image_png: result_image_png,
        })
    }

    /// Extract one prepared block window with fail-fast error handling.
    ///
    /// Inputs:
    /// - `jobs`: block crop requests already prepared as PNG bytes.
    /// - `limiter`: global VLM request limiter shared by all documents.
    async fn extract_window_blocks(
        &self,
        task: &ParseTask,
        session: &VlmSession,
        jobs: Vec<BlockExtractJob>,
        limiter: Arc<Semaphore>,
    ) -> ApiResult<Vec<BlockExtractResult>> {
        let mut extracts = Vec::with_capacity(jobs.len());
        let concurrency = jobs.len().min(self.vlm_max_concurrency).max(1);
        let mut pending = stream::iter(jobs.into_iter().map(|job| {
            self.extract_block(
                task,
                session,
                page_priority(job.page_index),
                job,
                limiter.clone(),
            )
        }))
        .buffer_unordered(concurrency);

        while let Some(result) = pending.next().await {
            match result {
                Ok(extract) => extracts.push(extract),
                Err(error) => {
                    tracing::debug!(
                        error = %error.detail(),
                        completed_extract_count = extracts.len(),
                        "block extraction failed; cancelling remaining window requests"
                    );
                    return Err(error);
                }
            }
        }
        Ok(extracts)
    }

    /// Execute one VLM request under the global concurrency limit.
    ///
    /// Inputs:
    /// - `limiter`: semaphore shared by all page and block requests in this process.
    /// - `request`: OpenAI-compatible chat completion payload data.
    async fn predict_with_limit(
        &self,
        limiter: &Arc<Semaphore>,
        session: &VlmSession,
        request: VlmRequest,
    ) -> ApiResult<String> {
        let wait_started_at = Instant::now();
        let _permit = limiter
            .clone()
            .acquire_owned()
            .await
            .map_err(|error| ApiError::Internal(error.to_string()))?;
        let wait_elapsed_ms = wait_started_at.elapsed().as_millis();
        let predict_started_at = Instant::now();
        let result = self.client.predict_with_session(session, request).await;
        let error_detail = result.as_ref().err().map(ApiError::detail);
        tracing::debug!(
            wait_permit_ms = wait_elapsed_ms,
            vlm_request_ms = predict_started_at.elapsed().as_millis(),
            ok = result.is_ok(),
            error = error_detail.as_deref(),
            "vlm request completed under limiter"
        );
        result
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
        let started_at = Instant::now();
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
        tracing::debug!(
            file_name = %document.file_name,
            image_count = document.image_files.len(),
            elapsed_ms = started_at.elapsed().as_millis(),
            "document files written"
        );
        Ok(())
    }

    async fn load_image_pages(&self, upload: &StoredUpload) -> ApiResult<Vec<RenderedPage>> {
        let started_at = Instant::now();
        let bytes = fs::read(&upload.path).await?;
        let image = image::load_from_memory(&bytes)
            .map_err(|error| ApiError::BadRequest(format!("Failed to load image: {error}")))?;
        tracing::debug!(
            file_name = %upload.stem,
            byte_len = bytes.len(),
            image_width = image.width(),
            image_height = image.height(),
            elapsed_ms = started_at.elapsed().as_millis(),
            "image upload loaded"
        );
        Ok(vec![RenderedPage {
            page_index: 0,
            point_width: image.width(),
            point_height: image.height(),
            image: Arc::new(image),
        }])
    }

    async fn load_pdf_page_window(
        &self,
        path: PathBuf,
        start_page_id: usize,
        end_page_id: usize,
    ) -> ApiResult<RenderedPageWindow> {
        let window_size = self.processing_window_size.max(1);
        let started_at = Instant::now();
        tokio::task::spawn_blocking(move || {
            render_pdf_page_window(&path, start_page_id, end_page_id, window_size)
        })
        .await
        .map_err(|error| ApiError::Internal(error.to_string()))?
        .inspect(|window| {
            tracing::debug!(
                start_page_id,
                end_page_id,
                window_size,
                rendered_pages = window.pages.len(),
                next_page_id = window.next_page_id,
                elapsed_ms = started_at.elapsed().as_millis(),
                "pdf render blocking task completed"
            );
        })
    }
}

/// Normalize image preprocessing thread count before building the dedicated CPU pool.
///
/// Inputs:
/// - `threads`: configured thread count after environment parsing.
fn normalize_image_preprocess_threads(threads: usize) -> usize {
    threads.clamp(1, MAX_IMAGE_PREPROCESS_THREADS)
}

/// Prepare one page layout detection PNG on the image thread pool.
///
/// Inputs:
/// - `pool`: shared CPU pool for resize and PNG encode work.
/// - `page`: rendered PDF or image page.
async fn prepare_page_layout_image_async(
    pool: Arc<rayon::ThreadPool>,
    page: RenderedPage,
) -> ApiResult<PreparedLayoutPage> {
    tokio::task::spawn_blocking(move || {
        pool.install(|| {
            let started_at = Instant::now();
            let image_png = encode_png(&prepare_layout_image(&page.image)?)?;
            Ok(PreparedLayoutPage {
                page,
                image_png,
                prepare_ms: started_at.elapsed().as_millis(),
            })
        })
    })
    .await
    .map_err(|error| ApiError::Internal(error.to_string()))?
}

/// Prepare block extraction jobs for one laid-out page on the image thread pool.
///
/// Inputs:
/// - `pool`: shared CPU pool for crop, resize, and PNG encode work.
/// - `image_analysis`: request option controlling skipped image/chart blocks.
/// - `layout`: page layout with source page image.
async fn prepare_page_extract_jobs_async(
    pool: Arc<rayon::ThreadPool>,
    image_analysis: bool,
    layout: &PageLayoutResult,
) -> ApiResult<Vec<BlockExtractJob>> {
    let layout_input = PageExtractInput {
        page_index: layout.page_index,
        page_image: layout.page_image.clone(),
        blocks: layout.blocks.clone(),
    };
    tokio::task::spawn_blocking(move || {
        pool.install(|| prepare_block_extract_jobs(image_analysis, layout_input))
    })
    .await
    .map_err(|error| ApiError::Internal(error.to_string()))?
}

struct PageExtractInput {
    page_index: usize,
    page_image: Arc<DynamicImage>,
    blocks: Vec<ContentBlock>,
}

/// Prepare all block extraction jobs for one page after layout detection.
///
/// Inputs:
/// - `image_analysis`: request option controlling skipped block types.
/// - `layout`: page image and detected blocks.
fn prepare_block_extract_jobs(
    image_analysis: bool,
    layout: PageExtractInput,
) -> ApiResult<Vec<BlockExtractJob>> {
    let skip_types = skip_extract_types(image_analysis);
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
    path: &Path,
    start_page_id: usize,
    end_page_id: usize,
    processing_window_size: usize,
) -> ApiResult<RenderedPageWindow> {
    let pdfium = bind_pdfium()?;
    let document = pdfium
        .load_pdf_from_file(path, None)
        .map_err(|error| ApiError::BadRequest(format!("Failed to open PDF: {error}")))?;
    let page_count = document.pages().len() as usize;
    let Some((start, window_end, next_page_id)) = pdf_page_window_bounds(
        page_count,
        start_page_id,
        end_page_id,
        processing_window_size,
    ) else {
        return Ok(RenderedPageWindow {
            pages: Vec::new(),
            next_page_id: start_page_id,
        });
    };
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
            image: Arc::new(image),
            point_width,
            point_height,
        });
    }
    Ok(RenderedPageWindow {
        pages: images,
        next_page_id,
    })
}

/// Calculate the inclusive PDF page range for one render window.
///
/// Inputs:
/// - `page_count`: actual PDF page count.
/// - `start_page_id`: zero-based requested start page.
/// - `end_page_id`: zero-based requested end page.
/// - `processing_window_size`: maximum pages to render in this window.
fn pdf_page_window_bounds(
    page_count: usize,
    start_page_id: usize,
    end_page_id: usize,
    processing_window_size: usize,
) -> Option<(usize, usize, usize)> {
    if page_count == 0 || start_page_id > end_page_id || start_page_id >= page_count {
        return None;
    }
    let start = start_page_id;
    let end = end_page_id.min(page_count - 1);
    let window_end = end.min(start.saturating_add(processing_window_size.max(1) - 1));
    Some((start, window_end, window_end.saturating_add(1)))
}

fn bind_pdfium() -> ApiResult<Pdfium> {
    pdfium_auto::bind_bundled().map_err(|error| {
        ApiError::Internal(format!(
            "Failed to bind bundled PDFium library: {error}. Rebuild the project so pdfium-auto can install the platform PDFium binary."
        ))
    })
}

fn prepare_layout_image(image: &DynamicImage) -> ApiResult<DynamicImage> {
    resize_layout_image_fast(image).or_else(|error| {
        tracing::trace!(
            error = %error.detail(),
            "fast layout resize failed; falling back to image crate"
        );
        Ok(image.resize_exact(
            LAYOUT_IMAGE_SIZE,
            LAYOUT_IMAGE_SIZE,
            imageops::FilterType::CatmullRom,
        ))
    })
}

fn resize_layout_image_fast(image: &DynamicImage) -> ApiResult<DynamicImage> {
    let source = image.to_rgba8();
    let src = FastImage::from_vec_u8(
        source.width(),
        source.height(),
        source.into_raw(),
        PixelType::U8x4,
    )
    .map_err(|error| ApiError::Internal(error.to_string()))?;
    let mut dst = FastImage::new(LAYOUT_IMAGE_SIZE, LAYOUT_IMAGE_SIZE, PixelType::U8x4);
    let options = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::CatmullRom));
    Resizer::new()
        .resize(&src, &mut dst, Some(&options))
        .map_err(|error| ApiError::Internal(error.to_string()))?;
    let buffer = image::RgbaImage::from_raw(LAYOUT_IMAGE_SIZE, LAYOUT_IMAGE_SIZE, dst.into_vec())
        .ok_or_else(|| {
        ApiError::Internal("Failed to build resized layout image".to_string())
    })?;
    Ok(DynamicImage::ImageRgba8(buffer))
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
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Arc,
        },
        time::Duration,
    };

    use axum::{extract::State, http::StatusCode, routing::get, routing::post, Json, Router};
    use chrono::Utc;
    use image::{DynamicImage, ImageBuffer, ImageFormat, Rgb};
    use printpdf::{Mm, PdfDocument, PdfPage, PdfSaveOptions};
    use serde_json::{json, Value};
    use tempfile::tempdir;
    use tokio::{net::TcpListener, sync::oneshot, time::sleep};
    use uuid::Uuid;

    use crate::domain::models::{ParseOptions, ParseTask, StoredUpload, TaskStatus};
    use crate::vlm::client::VlmHttpClient;
    use crate::vlm::python_compat::DocumentOutputAccumulator;
    use crate::vlm::test_env::{EnvVarGuard, TEST_ENV_LOCK};

    use super::{bind_pdfium, parse_layout_output, VlmDocumentParser};

    #[derive(Clone)]
    struct TestVlmState {
        models_count: Arc<AtomicUsize>,
        chat_count: Arc<AtomicUsize>,
        layout_chat_count: Arc<AtomicUsize>,
        text_chat_count: Arc<AtomicUsize>,
        active_layouts: Arc<AtomicUsize>,
        max_active_layouts: Arc<AtomicUsize>,
        completed_layouts: Arc<AtomicUsize>,
        text_before_all_layouts_completed: Arc<AtomicBool>,
        fail_layout_once: bool,
        fail_chat: bool,
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

    #[test]
    fn prepares_layout_image_with_expected_dimensions() {
        let image = DynamicImage::ImageRgb8(ImageBuffer::from_pixel(64, 32, Rgb([12_u8, 34, 56])));

        let prepared = super::prepare_layout_image(&image).expect("layout image prepares");
        let encoded = super::encode_png(&prepared).expect("layout image encodes");

        assert_eq!(prepared.width(), super::LAYOUT_IMAGE_SIZE);
        assert_eq!(prepared.height(), super::LAYOUT_IMAGE_SIZE);
        assert!(!encoded.is_empty());
    }

    #[test]
    fn pdf_page_window_bounds_stop_after_last_page() {
        assert_eq!(
            super::pdf_page_window_bounds(11, 0, 99999, 64),
            Some((0, 10, 11))
        );
        assert_eq!(super::pdf_page_window_bounds(11, 11, 99999, 64), None);
        assert_eq!(super::pdf_page_window_bounds(11, 12, 99999, 64), None);
    }

    #[tokio::test]
    async fn pdf_page_window_loads_from_file_path() {
        let temp = tempdir().expect("temp dir");
        let pdf_path = temp.path().join("sample.pdf");
        let mut warnings = Vec::new();
        let pdf_bytes = PdfDocument::new("path render test")
            .with_pages(vec![PdfPage::new(Mm(10.0), Mm(10.0), Vec::new())])
            .save(&PdfSaveOptions::default(), &mut warnings);
        tokio::fs::write(&pdf_path, pdf_bytes)
            .await
            .expect("pdf fixture should write");
        let parser =
            VlmDocumentParser::new(Arc::new(VlmHttpClient::new()), 4, 4, 2).expect("parser builds");

        let window = parser
            .load_pdf_page_window(pdf_path, 0, 99999)
            .await
            .expect("pdf window should render");

        assert_eq!(window.pages.len(), 1);
        assert_eq!(window.pages[0].page_index, 0);
        assert_eq!(window.next_page_id, 1);
    }

    #[tokio::test]
    async fn prepares_page_extract_jobs_with_stable_order() {
        let pool = Arc::new(
            rayon::ThreadPoolBuilder::new()
                .num_threads(2)
                .build()
                .expect("pool builds"),
        );
        let layout = super::PageLayoutResult {
            page_index: 2,
            page_width: 20,
            page_height: 20,
            point_width: 20,
            point_height: 20,
            page_image: Arc::new(DynamicImage::ImageRgb8(ImageBuffer::from_pixel(
                20,
                20,
                Rgb([255_u8, 255, 255]),
            ))),
            blocks: vec![
                super::ContentBlock {
                    block_type: "text".to_string(),
                    bbox: [0.0, 0.0, 0.5, 0.5],
                    angle: None,
                    content: None,
                    merge_prev: None,
                },
                super::ContentBlock {
                    block_type: "image".to_string(),
                    bbox: [0.5, 0.5, 1.0, 1.0],
                    angle: None,
                    content: None,
                    merge_prev: None,
                },
            ],
        };

        let jobs = super::prepare_page_extract_jobs_async(pool, true, &layout)
            .await
            .expect("jobs prepare");

        assert_eq!(jobs.len(), 2);
        assert_eq!(
            jobs.iter()
                .map(|job| (job.page_index, job.block_index, job.block_type.as_str()))
                .collect::<Vec<_>>(),
            vec![(2, 0, "text"), (2, 1, "image")]
        );
        assert!(jobs.iter().all(|job| !job.image_png.is_empty()));
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
        let parser =
            VlmDocumentParser::new(Arc::new(VlmHttpClient::new()), 4, 4, 2).expect("parser builds");

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
    async fn page_window_starts_page_blocks_before_all_layouts_finish() {
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
        let client = Arc::new(VlmHttpClient::new());
        let session = client
            .session_for_request(task.server_url.as_deref())
            .await
            .expect("session should resolve");
        let parser = VlmDocumentParser::new(client, 2, 2, 2).expect("parser builds");
        let pages = vec![
            rendered_test_page(1, [255, 0, 0]),
            rendered_test_page(0, [0, 255, 0]),
        ];

        let results = parser
            .parse_page_window(&task, &session, pages, temp.path().join("_pending_images"))
            .await
            .expect("window parse succeeds");

        server.abort();
        assert_eq!(
            results
                .iter()
                .map(|result| result.page_index())
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert_eq!(state.models_count.load(Ordering::SeqCst), 1);
        assert_eq!(state.chat_count.load(Ordering::SeqCst), 4);
        assert_eq!(state.max_active_layouts.load(Ordering::SeqCst), 2);
        assert!(
            state
                .text_before_all_layouts_completed
                .load(Ordering::SeqCst),
            "first page block extraction should start before the whole window layout stage finishes"
        );
        let mut builder = DocumentOutputAccumulator::new();
        builder.append_fragments(results);
        let output = builder.finish();
        assert!(output.markdown.contains("recognized text"));
    }

    #[tokio::test]
    async fn parser_uses_global_vlm_limit_across_concurrent_windows() {
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
        let client = Arc::new(VlmHttpClient::new());
        let session = client
            .session_for_request(task.server_url.as_deref())
            .await
            .expect("session should resolve");
        let parser = VlmDocumentParser::new(client, 2, 1, 2).expect("parser builds");

        let first = parser.parse_page_window(
            &task,
            &session,
            vec![rendered_test_page(0, [1, 2, 3])],
            temp.path().join("_pending_images_1"),
        );
        let second = parser.parse_page_window(
            &task,
            &session,
            vec![rendered_test_page(1, [4, 5, 6])],
            temp.path().join("_pending_images_2"),
        );
        let (first, second) = tokio::join!(first, second);

        server.abort();
        first.expect("first window should parse");
        second.expect("second window should parse");
        assert_eq!(state.max_active_layouts.load(Ordering::SeqCst), 1);
        assert_eq!(parser.available_vlm_permits(), 1);
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
        let parser =
            VlmDocumentParser::new(Arc::new(VlmHttpClient::new()), 4, 4, 2).expect("parser builds");

        let error = parser.parse_task(&task).await.expect_err("parse fails");

        server.abort();
        assert!(error.detail().contains("500 Internal Server Error"));
    }

    #[tokio::test]
    async fn page_window_fails_fast_on_layout_error() {
        let _env_lock = TEST_ENV_LOCK.lock().await;
        let _no_proxy = EnvVarGuard::set("NO_PROXY", "127.0.0.1,localhost");
        let (base_url, state, server) = spawn_test_vlm_server_with_layout_failure().await;
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
        let client = Arc::new(VlmHttpClient::new());
        let session = client
            .session_for_request(task.server_url.as_deref())
            .await
            .expect("session should resolve");
        let parser = VlmDocumentParser::new(client, 2, 1, 2).expect("parser builds");
        let pages = vec![
            rendered_test_page(0, [255, 0, 0]),
            rendered_test_page(1, [0, 255, 0]),
        ];

        let result = parser
            .parse_page_window(&task, &session, pages, temp.path().join("_pending_images"))
            .await;
        let error = match result {
            Ok(_) => panic!("layout error should fail the window"),
            Err(error) => error,
        };

        server.abort();
        assert!(
            error.detail().contains("400 Bad Request"),
            "{}",
            error.detail()
        );
        assert_eq!(state.layout_chat_count.load(Ordering::SeqCst), 1);
        assert_eq!(state.text_chat_count.load(Ordering::SeqCst), 0);
    }

    async fn spawn_test_vlm_server(
        fail_chat: bool,
    ) -> (String, TestVlmState, tokio::task::JoinHandle<()>) {
        let state = TestVlmState {
            models_count: Arc::new(AtomicUsize::new(0)),
            chat_count: Arc::new(AtomicUsize::new(0)),
            layout_chat_count: Arc::new(AtomicUsize::new(0)),
            text_chat_count: Arc::new(AtomicUsize::new(0)),
            active_layouts: Arc::new(AtomicUsize::new(0)),
            max_active_layouts: Arc::new(AtomicUsize::new(0)),
            completed_layouts: Arc::new(AtomicUsize::new(0)),
            text_before_all_layouts_completed: Arc::new(AtomicBool::new(false)),
            fail_layout_once: false,
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

    async fn spawn_test_vlm_server_with_layout_failure(
    ) -> (String, TestVlmState, tokio::task::JoinHandle<()>) {
        let state = TestVlmState {
            models_count: Arc::new(AtomicUsize::new(0)),
            chat_count: Arc::new(AtomicUsize::new(0)),
            layout_chat_count: Arc::new(AtomicUsize::new(0)),
            text_chat_count: Arc::new(AtomicUsize::new(0)),
            active_layouts: Arc::new(AtomicUsize::new(0)),
            max_active_layouts: Arc::new(AtomicUsize::new(0)),
            completed_layouts: Arc::new(AtomicUsize::new(0)),
            text_before_all_layouts_completed: Arc::new(AtomicBool::new(false)),
            fail_layout_once: true,
            fail_chat: false,
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
    ) -> (StatusCode, Json<Value>) {
        state.chat_count.fetch_add(1, Ordering::SeqCst);
        if state.fail_chat {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(openai_error_payload("mock chat failure", "server_error")),
            );
        }
        let prompt = extract_prompt_text(&payload);
        if prompt.contains("Layout Detection") {
            let layout_call = state.layout_chat_count.fetch_add(1, Ordering::SeqCst) + 1;
            if state.fail_layout_once {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(openai_error_payload("mock layout failure", "bad_request")),
                );
            }
            let active = state.active_layouts.fetch_add(1, Ordering::SeqCst) + 1;
            state.max_active_layouts.fetch_max(active, Ordering::SeqCst);
            let layout_delay_ms = if layout_call == 1 { 20 } else { 150 };
            sleep(Duration::from_millis(layout_delay_ms)).await;
            state.active_layouts.fetch_sub(1, Ordering::SeqCst);
            state.completed_layouts.fetch_add(1, Ordering::SeqCst);
            return (
                StatusCode::OK,
                Json(chat_payload(
                    "<|box_start|>0 0 1000 1000<|box_end|><|ref_start|>text<|ref_end|>",
                )),
            );
        }
        state.text_chat_count.fetch_add(1, Ordering::SeqCst);
        if state.completed_layouts.load(Ordering::SeqCst) < 2 {
            state
                .text_before_all_layouts_completed
                .store(true, Ordering::SeqCst);
        }
        (StatusCode::OK, Json(chat_payload("recognized text")))
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

    fn openai_error_payload(message: &str, code: &str) -> Value {
        json!({
            "error": {
                "message": message,
                "type": code,
                "code": code
            }
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
            image: Arc::new(image),
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
