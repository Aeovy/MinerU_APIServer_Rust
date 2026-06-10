use std::{
    io::{self, Cursor, Write},
    path::{Path, PathBuf},
    time::Instant,
};

use axum::{
    body::{Body, Bytes},
    http::{header, Response, StatusCode},
    response::IntoResponse,
    Json,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use crc32fast::Hasher;
use futures::stream;
use image::ImageFormat;
use printpdf::{Mm, Op, PdfDocument, PdfPage, PdfSaveOptions, RawImage, XObjectTransform};
use serde_json::{json, Map, Value};
use tokio::{fs, sync::mpsc};

use crate::{
    config::MINERU_VERSION,
    domain::{
        models::{DocumentKind, ParseTask},
        tasks::ResultReadLease,
    },
    error::{ApiError, ApiResult},
};

pub const FILE_PARSE_TASK_ID_HEADER: &str = "X-MinerU-Task-Id";
pub const FILE_PARSE_TASK_STATUS_HEADER: &str = "X-MinerU-Task-Status";
pub const FILE_PARSE_TASK_STATUS_URL_HEADER: &str = "X-MinerU-Task-Status-Url";
pub const FILE_PARSE_TASK_RESULT_URL_HEADER: &str = "X-MinerU-Task-Result-Url";
const ZIP_STREAM_CHANNEL_CAPACITY: usize = 8;
const ZIP_STREAM_CHUNK_SIZE: usize = 64 * 1024;
const ZIP_FLAG_DATA_DESCRIPTOR: u16 = 0x0008;
const ZIP_FLAG_UTF8_NAMES: u16 = 0x0800;
const ZIP_GENERAL_PURPOSE_FLAGS: u16 = ZIP_FLAG_DATA_DESCRIPTOR | ZIP_FLAG_UTF8_NAMES;

pub struct ResultBuilder;

impl ResultBuilder {
    /// Build a result response while optionally keeping the task output directory leased.
    ///
    /// Inputs:
    /// - `task`: completed task metadata and return flags.
    /// - `status_code`: HTTP response status to use.
    /// - `zip_filename`: download filename when ZIP output is requested.
    /// - `lease`: active result read lease held through JSON loading or ZIP streaming.
    pub async fn build_response_with_lease(
        task: &ParseTask,
        status_code: StatusCode,
        zip_filename: &str,
        lease: Option<ResultReadLease>,
    ) -> ApiResult<Response<Body>> {
        let started_at = Instant::now();
        if task.response_format_zip {
            let response = Response::builder()
                .status(status_code)
                .header(header::CONTENT_TYPE, "application/zip")
                .header(
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{zip_filename}\""),
                )
                .body(build_zip_stream(task, lease))
                .map_err(|error| ApiError::Internal(error.to_string()))?;
            tracing::debug!(
                task_id = %task.task_id,
                file_count = task.file_names.len(),
                zip_filename,
                elapsed_ms = started_at.elapsed().as_millis(),
                "zip response stream prepared"
            );
            return Ok(response);
        }

        let payload = Self::build_json_payload(task).await?;
        drop(lease);
        tracing::debug!(
            task_id = %task.task_id,
            file_count = task.file_names.len(),
            elapsed_ms = started_at.elapsed().as_millis(),
            "json response built"
        );
        Ok((status_code, Json(payload)).into_response())
    }

    /// Build the standard MinerU JSON result payload for a completed task.
    ///
    /// Inputs:
    /// - `task`: completed task metadata and return flags.
    pub async fn build_json_payload(task: &ParseTask) -> ApiResult<Value> {
        let started_at = Instant::now();
        let results = build_result_dict(task).await?;
        tracing::debug!(
            task_id = %task.task_id,
            file_count = task.file_names.len(),
            elapsed_ms = started_at.elapsed().as_millis(),
            "json payload built"
        );
        Ok(json!({
            "backend": task.backend,
            "version": MINERU_VERSION,
            "results": results
        }))
    }
}

async fn build_result_dict(task: &ParseTask) -> ApiResult<Value> {
    let started_at = Instant::now();
    let mut results = Map::new();
    for (file_index, file_name) in task.file_names.iter().enumerate() {
        let file_started_at = Instant::now();
        let parse_dir = parse_dir_for_task_file(task, file_index, file_name);
        let mut data = Map::new();
        let mut image_count = 0_usize;
        if task.return_md {
            data.insert(
                "md_content".to_string(),
                read_text(parse_dir.join(format!("{file_name}.md"))).await?,
            );
        }
        if task.return_middle_json {
            data.insert(
                "middle_json".to_string(),
                read_text(parse_dir.join(format!("{file_name}_middle.json"))).await?,
            );
        }
        if task.return_model_output {
            data.insert(
                "model_output".to_string(),
                read_text(parse_dir.join(format!("{file_name}_model.json"))).await?,
            );
        }
        if task.return_content_list {
            data.insert(
                "content_list".to_string(),
                read_text(parse_dir.join(format!("{file_name}_content_list.json"))).await?,
            );
        }
        if task.return_images {
            let images = read_images(&parse_dir.join("images")).await?;
            image_count = images.as_object().map(Map::len).unwrap_or_default();
            data.insert("images".to_string(), images);
        }
        let returned_field_count = data.len();
        results.insert(file_name.clone(), Value::Object(data));
        tracing::debug!(
            task_id = %task.task_id,
            file_name,
            returned_field_count,
            return_md = task.return_md,
            return_middle_json = task.return_middle_json,
            return_model_output = task.return_model_output,
            return_content_list = task.return_content_list,
            return_images = task.return_images,
            image_count,
            elapsed_ms = file_started_at.elapsed().as_millis(),
            "json result file loaded"
        );
    }
    tracing::debug!(
        task_id = %task.task_id,
        file_count = task.file_names.len(),
        elapsed_ms = started_at.elapsed().as_millis(),
        "json result dictionary built"
    );
    Ok(Value::Object(results))
}

fn parse_dir_for_task_file(task: &ParseTask, file_index: usize, file_name: &str) -> PathBuf {
    let kind = task
        .upload_suffixes
        .get(file_index)
        .and_then(|suffix| DocumentKind::from_suffix(suffix))
        .unwrap_or(DocumentKind::Pdf);
    task.output_dir.join(file_name).join(kind.output_subdir())
}

fn build_zip_stream(task: &ParseTask, lease: Option<ResultReadLease>) -> Body {
    let (sender, receiver) = mpsc::channel::<Result<Bytes, io::Error>>(ZIP_STREAM_CHANNEL_CAPACITY);
    let task = task.clone();
    tokio::task::spawn_blocking(move || {
        let _lease = lease;
        let started_at = Instant::now();
        let result = build_zip_to_writer(&task, ChannelZipWriter::new(sender.clone()));
        tracing::debug!(
            task_id = %task.task_id,
            file_count = task.file_names.len(),
            elapsed_ms = started_at.elapsed().as_millis(),
            ok = result.is_ok(),
            "zip stream build finished"
        );
        if let Err(error) = result {
            let _ = sender.blocking_send(Err(io::Error::other(error.detail())));
        }
    });
    let stream = stream::unfold(receiver, |mut receiver| async {
        receiver.recv().await.map(|item| (item, receiver))
    });
    Body::from_stream(stream)
}

#[cfg(test)]
async fn build_zip(task: &ParseTask) -> ApiResult<Vec<u8>> {
    let task = task.clone();
    tokio::task::spawn_blocking(move || -> ApiResult<Vec<u8>> {
        let cursor = build_zip_to_writer(&task, Cursor::new(Vec::new()))?;
        Ok(cursor.into_inner())
    })
    .await
    .map_err(|error| ApiError::Internal(error.to_string()))?
}

fn build_zip_to_writer<W: Write>(task: &ParseTask, writer: W) -> ApiResult<W> {
    let started_at = Instant::now();
    let mut writer = StreamingZipWriter::new(writer);
    for (file_index, file_name) in task.file_names.iter().enumerate() {
        let file_started_at = Instant::now();
        let parse_dir = parse_dir_for_task_file(task, file_index, file_name);
        add_if_requested(
            &mut writer,
            task.return_md,
            file_name,
            &parse_dir,
            &format!("{file_name}.md"),
        )?;
        add_if_requested(
            &mut writer,
            task.return_middle_json,
            file_name,
            &parse_dir,
            &format!("{file_name}_middle.json"),
        )?;
        add_if_requested(
            &mut writer,
            task.return_model_output,
            file_name,
            &parse_dir,
            &format!("{file_name}_model.json"),
        )?;
        add_if_requested(
            &mut writer,
            task.return_content_list,
            file_name,
            &parse_dir,
            &format!("{file_name}_content_list.json"),
        )?;
        add_if_requested(
            &mut writer,
            task.return_content_list,
            file_name,
            &parse_dir,
            &format!("{file_name}_content_list_v2.json"),
        )?;
        if task.return_images {
            let images_dir = parse_dir.join("images");
            if images_dir.is_dir() {
                for entry in std::fs::read_dir(&images_dir).map_err(|error| {
                    ApiError::internal_context(
                        format!(
                            "Failed to read result images directory: {}",
                            images_dir.display()
                        ),
                        error,
                    )
                })? {
                    let entry = entry.map_err(|error| {
                        ApiError::internal_context(
                            format!(
                                "Failed to read result image directory entry: {}",
                                images_dir.display()
                            ),
                            error,
                        )
                    })?;
                    let path = entry.path();
                    if path.is_file() {
                        let relative = format!(
                            "images/{}",
                            path.file_name()
                                .and_then(|name| name.to_str())
                                .unwrap_or("image")
                        );
                        add_file(&mut writer, file_name, &parse_dir, &relative)?;
                    }
                }
            }
        }
        if task.return_original_file {
            add_original_file(&mut writer, task, file_index, file_name)?;
        }
        tracing::debug!(
            task_id = %task.task_id,
            file_name,
            return_md = task.return_md,
            return_middle_json = task.return_middle_json,
            return_model_output = task.return_model_output,
            return_content_list = task.return_content_list,
            return_images = task.return_images,
            return_original_file = task.return_original_file,
            elapsed_ms = file_started_at.elapsed().as_millis(),
            "zip file entries added"
        );
    }
    let mut writer = writer.finish()?;
    writer
        .flush()
        .map_err(|error| ApiError::internal_context("Failed to flush ZIP response", error))?;
    tracing::debug!(
        task_id = %task.task_id,
        file_count = task.file_names.len(),
        elapsed_ms = started_at.elapsed().as_millis(),
        "zip writer finished"
    );
    Ok(writer)
}

fn add_if_requested<W: Write>(
    writer: &mut StreamingZipWriter<W>,
    requested: bool,
    file_name: &str,
    parse_dir: &Path,
    relative_path: &str,
) -> ApiResult<()> {
    if requested {
        add_file(writer, file_name, parse_dir, relative_path)?;
    }
    Ok(())
}

fn add_file<W: Write>(
    writer: &mut StreamingZipWriter<W>,
    file_name: &str,
    parse_dir: &Path,
    relative_path: &str,
) -> ApiResult<()> {
    let path = parse_dir.join(relative_path);
    if !path.exists() {
        return Ok(());
    }
    let output_subdir = parse_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("vlm");
    let arcname = format!("{file_name}/{output_subdir}/{relative_path}");
    add_path_as_file(writer, &path, &arcname)
}

fn add_original_file<W: Write>(
    writer: &mut StreamingZipWriter<W>,
    task: &ParseTask,
    file_index: usize,
    file_name: &str,
) -> ApiResult<()> {
    let (Some(upload_path), Some(suffix)) = (
        task.uploads.get(file_index),
        task.upload_suffixes.get(file_index),
    ) else {
        return Ok(());
    };
    if suffix == "pdf" {
        let arcname = format!("{file_name}/vlm/{file_name}_origin.pdf");
        return add_path_as_file(writer, upload_path, &arcname);
    }
    if DocumentKind::from_suffix(suffix) == Some(DocumentKind::Office) {
        let arcname = format!("{file_name}/office/{file_name}_origin.{suffix}");
        return add_path_as_file(writer, upload_path, &arcname);
    }
    let arcname = format!("{file_name}/vlm/{file_name}_origin.pdf");
    let source_bytes = std::fs::read(upload_path).map_err(|error| {
        ApiError::internal_context(
            format!(
                "Failed to read uploaded image for original-file PDF conversion: {}",
                upload_path.display()
            ),
            error,
        )
    })?;
    let bytes = image_to_pdf_bytes(&source_bytes)?;
    writer.add_bytes(&arcname, &bytes)
}

fn add_path_as_file<W: Write>(
    writer: &mut StreamingZipWriter<W>,
    path: &Path,
    arcname: &str,
) -> ApiResult<()> {
    let mut source = std::fs::File::open(path).map_err(|error| {
        ApiError::internal_context(
            format!(
                "Failed to open ZIP source file for {arcname}: {}",
                path.display()
            ),
            error,
        )
    })?;
    writer.add_reader(arcname, &mut source)
}

struct StreamingZipWriter<W: Write> {
    inner: W,
    entries: Vec<CentralDirectoryEntry>,
    position: u64,
}

struct CentralDirectoryEntry {
    name: Vec<u8>,
    crc32: u32,
    compressed_size: u32,
    uncompressed_size: u32,
    header_offset: u32,
}

impl<W: Write> StreamingZipWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            entries: Vec::new(),
            position: 0,
        }
    }

    fn add_bytes(&mut self, arcname: &str, bytes: &[u8]) -> ApiResult<()> {
        self.add_reader(arcname, &mut Cursor::new(bytes))
    }

    /// Add one ZIP entry using data descriptors so the body can be written without seeking.
    ///
    /// Inputs:
    /// - `arcname`: path stored in the ZIP archive.
    /// - `reader`: source data for the entry.
    fn add_reader<R: io::Read>(&mut self, arcname: &str, reader: &mut R) -> ApiResult<()> {
        let name = arcname.as_bytes().to_vec();
        let header_offset = u32::try_from(self.position)
            .map_err(|_| ApiError::Internal("ZIP archive exceeds 4 GiB".to_string()))?;
        self.write_local_header(&name)?;

        let mut hasher = Hasher::new();
        let mut uncompressed_size = 0_u64;
        let mut buffer = vec![0_u8; ZIP_STREAM_CHUNK_SIZE];
        loop {
            let read = reader.read(&mut buffer).map_err(|error| {
                ApiError::internal_context(
                    format!("Failed to read ZIP entry source: {arcname}"),
                    error,
                )
            })?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            uncompressed_size += read as u64;
            self.write_counted(&buffer[..read])?;
        }

        let size = u32::try_from(uncompressed_size)
            .map_err(|_| ApiError::Internal("ZIP entry exceeds 4 GiB".to_string()))?;
        let crc32 = hasher.finalize();
        self.write_data_descriptor(crc32, size)?;
        self.entries.push(CentralDirectoryEntry {
            name,
            crc32,
            compressed_size: size,
            uncompressed_size: size,
            header_offset,
        });
        Ok(())
    }

    fn finish(mut self) -> ApiResult<W> {
        let central_offset = u32::try_from(self.position)
            .map_err(|_| ApiError::Internal("ZIP archive exceeds 4 GiB".to_string()))?;
        for index in 0..self.entries.len() {
            self.write_central_directory_entry(index)?;
        }
        let central_size = u32::try_from(self.position - u64::from(central_offset))
            .map_err(|_| ApiError::Internal("ZIP central directory exceeds 4 GiB".to_string()))?;
        self.write_end_of_central_directory(central_size, central_offset)?;
        Ok(self.inner)
    }

    fn write_local_header(&mut self, name: &[u8]) -> ApiResult<()> {
        self.write_u32(0x0403_4b50)?;
        self.write_u16(20)?;
        self.write_u16(ZIP_GENERAL_PURPOSE_FLAGS)?;
        self.write_u16(0)?;
        self.write_u16(0)?;
        self.write_u16(0)?;
        self.write_u32(0)?;
        self.write_u32(0)?;
        self.write_u32(0)?;
        self.write_u16(name_len(name)?)?;
        self.write_u16(0)?;
        self.write_counted(name)
    }

    fn write_data_descriptor(&mut self, crc32: u32, size: u32) -> ApiResult<()> {
        self.write_u32(0x0807_4b50)?;
        self.write_u32(crc32)?;
        self.write_u32(size)?;
        self.write_u32(size)
    }

    fn write_central_directory_entry(&mut self, index: usize) -> ApiResult<()> {
        let (name, crc32, compressed_size, uncompressed_size, header_offset) = {
            let entry = &self.entries[index];
            (
                entry.name.clone(),
                entry.crc32,
                entry.compressed_size,
                entry.uncompressed_size,
                entry.header_offset,
            )
        };
        self.write_u32(0x0201_4b50)?;
        self.write_u16(20)?;
        self.write_u16(20)?;
        self.write_u16(ZIP_GENERAL_PURPOSE_FLAGS)?;
        self.write_u16(0)?;
        self.write_u16(0)?;
        self.write_u16(0)?;
        self.write_u32(crc32)?;
        self.write_u32(compressed_size)?;
        self.write_u32(uncompressed_size)?;
        self.write_u16(name_len(&name)?)?;
        self.write_u16(0)?;
        self.write_u16(0)?;
        self.write_u16(0)?;
        self.write_u16(0)?;
        self.write_u32(0)?;
        self.write_u32(header_offset)?;
        self.write_counted(&name)
    }

    fn write_end_of_central_directory(
        &mut self,
        central_size: u32,
        central_offset: u32,
    ) -> ApiResult<()> {
        let entry_count = u16::try_from(self.entries.len())
            .map_err(|_| ApiError::Internal("ZIP archive has too many entries".to_string()))?;
        self.write_u32(0x0605_4b50)?;
        self.write_u16(0)?;
        self.write_u16(0)?;
        self.write_u16(entry_count)?;
        self.write_u16(entry_count)?;
        self.write_u32(central_size)?;
        self.write_u32(central_offset)?;
        self.write_u16(0)
    }

    fn write_u16(&mut self, value: u16) -> ApiResult<()> {
        self.write_counted(&value.to_le_bytes())
    }

    fn write_u32(&mut self, value: u32) -> ApiResult<()> {
        self.write_counted(&value.to_le_bytes())
    }

    fn write_counted(&mut self, bytes: &[u8]) -> ApiResult<()> {
        self.inner.write_all(bytes).map_err(|error| {
            ApiError::internal_context("Failed to write ZIP response bytes", error)
        })?;
        self.position += bytes.len() as u64;
        Ok(())
    }
}

fn name_len(name: &[u8]) -> ApiResult<u16> {
    u16::try_from(name.len()).map_err(|_| ApiError::Internal("ZIP path is too long".to_string()))
}

struct ChannelZipWriter {
    sender: mpsc::Sender<Result<Bytes, io::Error>>,
    buffer: Vec<u8>,
}

impl ChannelZipWriter {
    fn new(sender: mpsc::Sender<Result<Bytes, io::Error>>) -> Self {
        Self {
            sender,
            buffer: Vec::with_capacity(ZIP_STREAM_CHUNK_SIZE),
        }
    }

    fn flush_buffer(&mut self) -> io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let chunk = Bytes::from(std::mem::take(&mut self.buffer));
        self.sender
            .blocking_send(Ok(chunk))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "zip response stream closed"))
    }
}

impl Write for ChannelZipWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(buf);
        if self.buffer.len() >= ZIP_STREAM_CHUNK_SIZE {
            self.flush_buffer()?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_buffer()
    }
}

/// Convert one uploaded image into the PDF original file shape used by Python MinerU.
///
/// Inputs:
/// - `image_bytes`: original uploaded image bytes.
fn image_to_pdf_bytes(image_bytes: &[u8]) -> ApiResult<Vec<u8>> {
    const IMAGE_PDF_DPI: f32 = 200.0;
    let image = image::load_from_memory(image_bytes)
        .map_err(|error| ApiError::BadRequest(format!("Failed to load image: {error}")))?;
    let rgb_image = image.to_rgb8();
    let width = rgb_image.width();
    let height = rgb_image.height();
    let mut png_bytes = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(rgb_image)
        .write_to(&mut png_bytes, ImageFormat::Png)
        .map_err(|error| ApiError::Internal(error.to_string()))?;

    let mut warnings = Vec::new();
    let raw_image = RawImage::decode_from_bytes(&png_bytes.into_inner(), &mut warnings)
        .map_err(ApiError::Internal)?;
    let mut doc = PdfDocument::new("MinerU original image");
    let image_id = doc.add_image(&raw_image);
    let page_width_mm = width as f32 * 25.4 / IMAGE_PDF_DPI;
    let page_height_mm = height as f32 * 25.4 / IMAGE_PDF_DPI;
    let page = PdfPage::new(
        Mm(page_width_mm),
        Mm(page_height_mm),
        vec![Op::UseXobject {
            id: image_id,
            transform: XObjectTransform {
                dpi: Some(IMAGE_PDF_DPI),
                ..Default::default()
            },
        }],
    );
    Ok(doc
        .with_pages(vec![page])
        .save(&PdfSaveOptions::default(), &mut warnings))
}

async fn read_text(path: PathBuf) -> ApiResult<Value> {
    if !path.exists() {
        return Ok(Value::Null);
    }
    Ok(Value::String(fs::read_to_string(&path).await.map_err(
        |error| {
            ApiError::internal_context(
                format!("Failed to read result text file: {}", path.display()),
                error,
            )
        },
    )?))
}

async fn read_images(images_dir: &Path) -> ApiResult<Value> {
    let mut images = Map::new();
    if !images_dir.is_dir() {
        return Ok(Value::Object(images));
    }
    let mut entries = fs::read_dir(images_dir).await.map_err(|error| {
        ApiError::internal_context(
            format!(
                "Failed to read result images directory: {}",
                images_dir.display()
            ),
            error,
        )
    })?;
    while let Some(entry) = entries.next_entry().await.map_err(|error| {
        ApiError::internal_context(
            format!(
                "Failed to iterate result images directory: {}",
                images_dir.display()
            ),
            error,
        )
    })? {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("image")
            .to_string();
        let mime = mime_guess::from_path(&path)
            .first_raw()
            .unwrap_or("image/jpeg");
        let bytes = fs::read(&path).await.map_err(|error| {
            ApiError::internal_context(
                format!("Failed to read result image file: {}", path.display()),
                error,
            )
        })?;
        images.insert(
            name,
            Value::String(format!("data:{mime};base64,{}", STANDARD.encode(bytes))),
        );
    }
    Ok(Value::Object(images))
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Read};

    use axum::{
        body::to_bytes,
        http::{header, Request, StatusCode},
        routing::get,
        Router,
    };
    use chrono::Utc;
    use image::{ImageBuffer, ImageFormat, Rgb};
    use tempfile::tempdir;
    use tower::ServiceExt;
    use tower_http::compression::{
        predicate::{DefaultPredicate, NotForContentType, Predicate},
        CompressionLayer,
    };
    use uuid::Uuid;
    use zip::ZipArchive;

    use crate::domain::models::{ParseTask, TaskStatus};

    use super::{
        build_zip, ResultBuilder, ZIP_FLAG_DATA_DESCRIPTOR, ZIP_FLAG_UTF8_NAMES,
        ZIP_GENERAL_PURPOSE_FLAGS,
    };

    #[tokio::test]
    async fn image_original_is_zipped_as_pdf() {
        let temp = tempdir().expect("temp dir");
        let upload_path = temp.path().join("sample.png");
        let image = ImageBuffer::from_pixel(4, 3, Rgb([255_u8, 0, 0]));
        image
            .save_with_format(&upload_path, ImageFormat::Png)
            .expect("png fixture");
        let task = completed_zip_task(temp.path().to_path_buf(), upload_path);

        let bytes = build_zip(&task).await.expect("zip bytes");
        let mut archive = ZipArchive::new(Cursor::new(bytes)).expect("zip archive");
        assert!(archive.by_name("sample/vlm/sample_origin.png").is_err());
        let mut original = archive
            .by_name("sample/vlm/sample_origin.pdf")
            .expect("python-compatible image original path");
        let mut original_bytes = Vec::new();
        original.read_to_end(&mut original_bytes).expect("read pdf");
        assert!(original_bytes.starts_with(b"%PDF"));
    }

    #[tokio::test]
    async fn zip_response_streams_without_content_length() {
        let temp = tempdir().expect("temp dir");
        let upload_path = temp.path().join("sample.png");
        let image = ImageBuffer::from_pixel(4, 3, Rgb([255_u8, 0, 0]));
        image
            .save_with_format(&upload_path, ImageFormat::Png)
            .expect("png fixture");
        let task = completed_zip_task(temp.path().to_path_buf(), upload_path);

        let response =
            ResultBuilder::build_response_with_lease(&task, StatusCode::OK, "sample.zip", None)
                .await
                .expect("zip response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/zip"
        );
        assert_eq!(
            response.headers().get(header::CONTENT_DISPOSITION).unwrap(),
            "attachment; filename=\"sample.zip\""
        );
        assert!(response.headers().get(header::CONTENT_LENGTH).is_none());

        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("streamed body");
        let mut archive = ZipArchive::new(Cursor::new(bytes)).expect("zip archive");
        let mut original = archive
            .by_name("sample/vlm/sample_origin.pdf")
            .expect("python-compatible image original path");
        let mut original_bytes = Vec::new();
        original.read_to_end(&mut original_bytes).expect("read pdf");
        assert!(original_bytes.starts_with(b"%PDF"));
    }

    #[tokio::test]
    async fn zip_entries_preserve_chinese_file_names() {
        let temp = tempdir().expect("temp dir");
        let file_name = "解析测试文档";
        let parse_dir = temp.path().join(file_name).join("vlm");
        tokio::fs::create_dir_all(&parse_dir)
            .await
            .expect("parse dir should be created");
        tokio::fs::write(parse_dir.join(format!("{file_name}.md")), "# ok\n")
            .await
            .expect("markdown should write");
        let mut task = completed_zip_task(temp.path().to_path_buf(), temp.path().join("noop.pdf"));
        task.file_names = vec![file_name.to_string()];
        task.return_md = true;
        task.return_original_file = false;

        let bytes = build_zip(&task).await.expect("zip bytes");
        assert_zip_utf8_flags(&bytes);
        let mut archive = ZipArchive::new(Cursor::new(bytes)).expect("zip archive");
        let mut markdown = archive
            .by_name("解析测试文档/vlm/解析测试文档.md")
            .expect("utf-8 zip entry should be readable by name");
        let mut content = String::new();
        markdown
            .read_to_string(&mut content)
            .expect("markdown should read");

        assert_eq!(content, "# ok\n");
    }

    #[tokio::test]
    async fn json_result_reads_office_output_dir() {
        let temp = tempdir().expect("temp dir");
        let parse_dir = temp.path().join("sample").join("office");
        tokio::fs::create_dir_all(&parse_dir)
            .await
            .expect("office parse dir should be created");
        tokio::fs::write(parse_dir.join("sample.md"), "# office\n")
            .await
            .expect("office markdown should write");
        let mut task =
            completed_zip_task(temp.path().to_path_buf(), temp.path().join("sample.docx"));
        task.response_format_zip = false;
        task.return_md = true;
        task.return_original_file = false;
        task.upload_suffixes = vec!["docx".to_string()];

        let payload = ResultBuilder::build_json_payload(&task)
            .await
            .expect("json payload should build");
        assert_eq!(
            payload["results"]["sample"]["md_content"],
            serde_json::Value::String("# office\n".to_string())
        );
    }

    #[tokio::test]
    async fn zip_result_reads_office_output_dir() {
        let temp = tempdir().expect("temp dir");
        let parse_dir = temp.path().join("sample").join("office");
        tokio::fs::create_dir_all(&parse_dir)
            .await
            .expect("office parse dir should be created");
        tokio::fs::write(parse_dir.join("sample.md"), "# office\n")
            .await
            .expect("office markdown should write");
        let mut task =
            completed_zip_task(temp.path().to_path_buf(), temp.path().join("sample.docx"));
        task.return_md = true;
        task.return_original_file = false;
        task.upload_suffixes = vec!["docx".to_string()];

        let bytes = build_zip(&task).await.expect("zip bytes");
        let mut archive = ZipArchive::new(Cursor::new(bytes)).expect("zip archive");
        let mut markdown = archive
            .by_name("sample/office/sample.md")
            .expect("office markdown entry should exist");
        let mut content = String::new();
        markdown
            .read_to_string(&mut content)
            .expect("markdown should read");
        assert_eq!(content, "# office\n");
    }

    #[tokio::test]
    async fn office_original_is_zipped_as_original_file() {
        let temp = tempdir().expect("temp dir");
        let parse_dir = temp.path().join("sample").join("office");
        tokio::fs::create_dir_all(&parse_dir)
            .await
            .expect("office parse dir should be created");
        tokio::fs::write(parse_dir.join("sample.md"), "# office\n")
            .await
            .expect("office markdown should write");
        let upload_path = temp.path().join("sample.docx");
        tokio::fs::write(&upload_path, b"docx-bytes")
            .await
            .expect("original should write");
        let mut task = completed_zip_task(temp.path().to_path_buf(), upload_path);
        task.return_md = true;
        task.return_original_file = true;
        task.upload_suffixes = vec!["docx".to_string()];

        let bytes = build_zip(&task).await.expect("zip bytes");
        let mut archive = ZipArchive::new(Cursor::new(bytes)).expect("zip archive");
        assert!(archive.by_name("sample/vlm/sample_origin.pdf").is_err());
        let mut original = archive
            .by_name("sample/office/sample_origin.docx")
            .expect("office original entry should exist");
        let mut content = Vec::new();
        original
            .read_to_end(&mut content)
            .expect("original should read");
        assert_eq!(content, b"docx-bytes");
    }

    #[tokio::test]
    async fn zip_response_is_not_http_gzip_compressed() {
        let temp = tempdir().expect("temp dir");
        let upload_path = temp.path().join("sample.png");
        let image = ImageBuffer::from_pixel(4, 3, Rgb([255_u8, 0, 0]));
        image
            .save_with_format(&upload_path, ImageFormat::Png)
            .expect("png fixture");
        let task = completed_zip_task(temp.path().to_path_buf(), upload_path);
        let app = Router::new()
            .route(
                "/zip",
                get(move || {
                    let task = task.clone();
                    async move {
                        ResultBuilder::build_response_with_lease(
                            &task,
                            StatusCode::OK,
                            "sample.zip",
                            None,
                        )
                        .await
                    }
                }),
            )
            .layer(CompressionLayer::new().compress_when(
                DefaultPredicate::new().and(NotForContentType::const_new("application/zip")),
            ));

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/zip")
                    .header(header::ACCEPT_ENCODING, "gzip")
                    .body(axum::body::Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("zip route should respond");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/zip"
        );
        assert!(response.headers().get(header::CONTENT_ENCODING).is_none());
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body should read");
        assert!(bytes.starts_with(b"PK\x03\x04"));
        assert!(!bytes.starts_with(&[0x1f, 0x8b, 0x08]));
    }

    fn assert_zip_utf8_flags(bytes: &[u8]) {
        let local_flags = u16::from_le_bytes([bytes[6], bytes[7]]);
        assert_eq!(local_flags, ZIP_GENERAL_PURPOSE_FLAGS);
        assert_ne!(local_flags & ZIP_FLAG_DATA_DESCRIPTOR, 0);
        assert_ne!(local_flags & ZIP_FLAG_UTF8_NAMES, 0);

        let central_offset = bytes
            .windows(4)
            .position(|window| window == b"PK\x01\x02")
            .expect("central directory header should exist");
        let central_flags =
            u16::from_le_bytes([bytes[central_offset + 8], bytes[central_offset + 9]]);
        assert_eq!(central_flags, ZIP_GENERAL_PURPOSE_FLAGS);
        assert_ne!(central_flags & ZIP_FLAG_DATA_DESCRIPTOR, 0);
        assert_ne!(central_flags & ZIP_FLAG_UTF8_NAMES, 0);
    }

    fn completed_zip_task(
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
            return_md: false,
            return_middle_json: false,
            return_model_output: false,
            return_content_list: false,
            return_images: false,
            response_format_zip: true,
            return_original_file: true,
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
