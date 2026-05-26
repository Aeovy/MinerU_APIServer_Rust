use std::{
    io::{self, Cursor, Write},
    path::{Path, PathBuf},
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
    domain::models::ParseTask,
    error::{ApiError, ApiResult},
};

pub const FILE_PARSE_TASK_ID_HEADER: &str = "X-MinerU-Task-Id";
pub const FILE_PARSE_TASK_STATUS_HEADER: &str = "X-MinerU-Task-Status";
pub const FILE_PARSE_TASK_STATUS_URL_HEADER: &str = "X-MinerU-Task-Status-Url";
pub const FILE_PARSE_TASK_RESULT_URL_HEADER: &str = "X-MinerU-Task-Result-Url";
const ZIP_STREAM_CHANNEL_CAPACITY: usize = 8;
const ZIP_STREAM_CHUNK_SIZE: usize = 64 * 1024;

pub struct ResultBuilder;

impl ResultBuilder {
    /// Build a JSON or ZIP HTTP response for a completed parse task.
    ///
    /// Inputs:
    /// - `task`: completed task metadata and return flags.
    /// - `status_code`: HTTP response status to use.
    /// - `zip_filename`: download filename when ZIP output is requested.
    pub async fn build_response(
        task: &ParseTask,
        status_code: StatusCode,
        zip_filename: &str,
    ) -> ApiResult<Response<Body>> {
        if task.response_format_zip {
            let response = Response::builder()
                .status(status_code)
                .header(header::CONTENT_TYPE, "application/zip")
                .header(
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{zip_filename}\""),
                )
                .body(build_zip_stream(task))
                .map_err(|error| ApiError::Internal(error.to_string()))?;
            return Ok(response);
        }

        let payload = Self::build_json_payload(task).await?;
        Ok((status_code, Json(payload)).into_response())
    }

    /// Build the standard MinerU JSON result payload for a completed task.
    ///
    /// Inputs:
    /// - `task`: completed task metadata and return flags.
    pub async fn build_json_payload(task: &ParseTask) -> ApiResult<Value> {
        let results = build_result_dict(task).await?;
        Ok(json!({
            "backend": task.backend,
            "version": MINERU_VERSION,
            "results": results
        }))
    }
}

async fn build_result_dict(task: &ParseTask) -> ApiResult<Value> {
    let mut results = Map::new();
    for file_name in &task.file_names {
        let parse_dir = task.output_dir.join(file_name).join("vlm");
        let mut data = Map::new();
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
            data.insert(
                "images".to_string(),
                read_images(&parse_dir.join("images")).await?,
            );
        }
        results.insert(file_name.clone(), Value::Object(data));
    }
    Ok(Value::Object(results))
}

fn build_zip_stream(task: &ParseTask) -> Body {
    let (sender, receiver) = mpsc::channel::<Result<Bytes, io::Error>>(ZIP_STREAM_CHANNEL_CAPACITY);
    let task = task.clone();
    tokio::task::spawn_blocking(move || {
        let result = build_zip_to_writer(&task, ChannelZipWriter::new(sender.clone()));
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
    let mut writer = StreamingZipWriter::new(writer);
    for (file_index, file_name) in task.file_names.iter().enumerate() {
        let parse_dir = task.output_dir.join(file_name).join("vlm");
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
                for entry in std::fs::read_dir(&images_dir).map_err(ApiError::from)? {
                    let entry = entry.map_err(ApiError::from)?;
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
    }
    let mut writer = writer.finish()?;
    writer.flush().map_err(ApiError::from)?;
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
    let arcname = format!("{file_name}/vlm/{relative_path}");
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
    let arcname = format!("{file_name}/vlm/{file_name}_origin.pdf");
    if suffix == "pdf" {
        return add_path_as_file(writer, upload_path, &arcname);
    }
    let source_bytes = std::fs::read(upload_path).map_err(ApiError::from)?;
    let bytes = image_to_pdf_bytes(&source_bytes)?;
    writer.add_bytes(&arcname, &bytes)
}

fn add_path_as_file<W: Write>(
    writer: &mut StreamingZipWriter<W>,
    path: &Path,
    arcname: &str,
) -> ApiResult<()> {
    let mut source = std::fs::File::open(path).map_err(ApiError::from)?;
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
            let read = reader.read(&mut buffer).map_err(ApiError::from)?;
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
        self.write_u16(0x08)?;
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
        self.write_u16(0x08)?;
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
        self.inner.write_all(bytes).map_err(ApiError::from)?;
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
    Ok(Value::String(fs::read_to_string(path).await?))
}

async fn read_images(images_dir: &Path) -> ApiResult<Value> {
    let mut images = Map::new();
    if !images_dir.is_dir() {
        return Ok(Value::Object(images));
    }
    let mut entries = fs::read_dir(images_dir).await?;
    while let Some(entry) = entries.next_entry().await? {
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
        let bytes = fs::read(path).await?;
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
        http::{header, StatusCode},
    };
    use chrono::Utc;
    use image::{ImageBuffer, ImageFormat, Rgb};
    use tempfile::tempdir;
    use uuid::Uuid;
    use zip::ZipArchive;

    use crate::domain::models::{ParseTask, TaskStatus};

    use super::{build_zip, ResultBuilder};

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

        let response = ResultBuilder::build_response(&task, StatusCode::OK, "sample.zip")
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
