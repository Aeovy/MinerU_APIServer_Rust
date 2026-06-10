use std::path::Path;

use base64::{engine::general_purpose::STANDARD, Engine};
use image::{
    codecs::jpeg::JpegEncoder, imageops::overlay, ImageBuffer, ImageFormat, Rgb, RgbImage,
};
use sha2::{Digest, Sha256};

use crate::error::{ApiError, ApiResult};

const PLACEHOLDER_WIDTH: u32 = 320;
const PLACEHOLDER_HEIGHT: u32 = 180;
const PLACEHOLDER_BACKGROUND: Rgb<u8> = Rgb([240, 240, 240]);
const PLACEHOLDER_BORDER: Rgb<u8> = Rgb([190, 190, 190]);
const PLACEHOLDER_TEXT: Rgb<u8> = Rgb([90, 90, 90]);

#[derive(Debug, Clone)]
pub struct SerializedOfficeImage {
    pub file_name: String,
    pub bytes: Vec<u8>,
    pub warning: Option<String>,
}

/// Serialize one Office media part into a web-consumable image payload.
///
/// Inputs:
/// - `suggested_name`: OOXML media part file name.
/// - `content_type`: optional content type from `[Content_Types].xml`.
/// - `bytes`: media bytes extracted from the OOXML package.
pub fn serialize_office_image(
    suggested_name: &str,
    content_type: Option<&str>,
    bytes: &[u8],
) -> ApiResult<Option<SerializedOfficeImage>> {
    if is_vector_image(suggested_name, content_type) {
        let label = vector_format_label(suggested_name, content_type);
        let placeholder = placeholder_jpeg(&[
            format!("{label} placeholder"),
            "Use Windows to parse".to_string(),
            "the original image".to_string(),
        ])?;
        return Ok(Some(SerializedOfficeImage::from_bytes(
            "jpeg",
            placeholder,
            Some(format!(
                "unsupported_vector_image: generated {label} placeholder for {suggested_name}"
            )),
        )));
    }

    let Some(format) = detect_raster_format(suggested_name, content_type, bytes) else {
        tracing::warn!(suggested_name, "unsupported Office image format");
        return Ok(None);
    };
    Ok(Some(SerializedOfficeImage::from_bytes(
        format,
        bytes.to_vec(),
        None,
    )))
}

impl SerializedOfficeImage {
    fn from_bytes(format: &str, bytes: Vec<u8>, warning: Option<String>) -> Self {
        let extension = extension_for_format(format);
        let media_type = media_type_for_format(format);
        let file_name = hashed_file_name(media_type, &bytes, extension);
        Self {
            file_name,
            bytes,
            warning,
        }
    }
}

fn detect_raster_format(
    suggested_name: &str,
    content_type: Option<&str>,
    bytes: &[u8],
) -> Option<&'static str> {
    let extension = Path::new(suggested_name)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if extension == "svg" && looks_like_svg(bytes) {
        return Some("svg");
    }
    let type_format = format_from_content_type(content_type);
    let format = format_from_magic(bytes).or_else(|| format_from_image_crate(bytes))?;
    if let Some(type_format) = type_format {
        if type_format != format {
            return None;
        }
    }
    is_extension_compatible(&extension, format).then_some(format)
}

fn format_from_magic(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("jpeg");
    }
    if bytes.starts_with(b"\x89PNG\r\n\x1A\n") {
        return Some("png");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("gif");
    }
    if bytes.starts_with(b"BM") {
        return Some("bmp");
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("webp");
    }
    let prefix = String::from_utf8_lossy(&bytes[..bytes.len().min(256)]).to_ascii_lowercase();
    if prefix.contains("<svg") {
        return Some("svg");
    }
    None
}

fn format_from_image_crate(bytes: &[u8]) -> Option<&'static str> {
    match image::guess_format(bytes).ok()? {
        ImageFormat::Png => Some("png"),
        ImageFormat::Jpeg => Some("jpeg"),
        ImageFormat::Gif => Some("gif"),
        ImageFormat::WebP => Some("webp"),
        ImageFormat::Bmp => Some("bmp"),
        _ => None,
    }
}

fn looks_like_svg(bytes: &[u8]) -> bool {
    String::from_utf8_lossy(&bytes[..bytes.len().min(512)])
        .to_ascii_lowercase()
        .contains("<svg")
}

fn is_extension_compatible(extension: &str, format: &str) -> bool {
    if extension.is_empty() {
        return true;
    }
    matches!(
        (extension, format),
        ("jpg" | "jpeg", "jpeg")
            | ("png", "png")
            | ("gif", "gif")
            | ("webp", "webp")
            | ("bmp", "bmp")
            | ("svg", "svg")
    )
}

fn format_from_content_type(content_type: Option<&str>) -> Option<&'static str> {
    let normalized = content_type?.split(';').next()?.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "image/jpeg" | "image/jpg" => Some("jpeg"),
        "image/png" => Some("png"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        "image/bmp" | "image/x-ms-bmp" => Some("bmp"),
        "image/svg+xml" => Some("svg"),
        _ => None,
    }
}

fn is_vector_image(name: &str, content_type: Option<&str>) -> bool {
    let extension_matches = matches!(
        Path::new(name)
            .extension()
            .and_then(|value| value.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("emf" | "wmf")
    );
    if extension_matches {
        return true;
    }
    matches!(
        content_type
            .and_then(|value| value.split(';').next())
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some(
            "image/x-wmf" | "image/wmf" | "image/x-emf" | "image/emf" | "application/x-msmetafile"
        )
    )
}

fn vector_format_label(name: &str, content_type: Option<&str>) -> &'static str {
    match Path::new(name)
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("wmf") => "WMF",
        Some("emf") => "EMF",
        _ => match content_type.map(str::to_ascii_lowercase).as_deref() {
            Some(value) if value.contains("wmf") => "WMF",
            Some(value) if value.contains("emf") || value.contains("msmetafile") => "EMF",
            _ => "WMF/EMF",
        },
    }
}

fn extension_for_format(format: &str) -> &'static str {
    match format.to_ascii_lowercase().as_str() {
        "jpeg" | "jpg" => "jpg",
        "png" => "png",
        "gif" => "gif",
        "webp" => "webp",
        "bmp" => "bmp",
        "svg" => "svg",
        _ => "bin",
    }
}

fn media_type_for_format(format: &str) -> &'static str {
    match format.to_ascii_lowercase().as_str() {
        "jpeg" | "jpg" => "jpeg",
        "png" => "png",
        "gif" => "gif",
        "webp" => "webp",
        "bmp" => "bmp",
        "svg" => "svg+xml",
        _ => "octet-stream",
    }
}

fn hashed_file_name(media_type: &str, bytes: &[u8], extension: &str) -> String {
    let b64 = STANDARD.encode(bytes);
    let data_uri = format!("data:image/{media_type};base64,{b64}");
    let mut hasher = Sha256::new();
    hasher.update(data_uri.as_bytes());
    format!("{:x}.{extension}", hasher.finalize())
}

fn placeholder_jpeg(lines: &[String]) -> ApiResult<Vec<u8>> {
    let mut image = ImageBuffer::from_pixel(
        PLACEHOLDER_WIDTH,
        PLACEHOLDER_HEIGHT,
        PLACEHOLDER_BACKGROUND,
    );
    draw_border(&mut image);
    draw_centered_text(&mut image, lines);

    let mut bytes = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut bytes, 85);
    encoder
        .encode_image(&image)
        .map_err(|error| ApiError::Internal(error.to_string()))?;
    Ok(bytes)
}

fn draw_border(image: &mut RgbImage) {
    let width = image.width();
    let height = image.height();
    for x in 0..width {
        image.put_pixel(x, 0, PLACEHOLDER_BORDER);
        image.put_pixel(x, height - 1, PLACEHOLDER_BORDER);
    }
    for y in 0..height {
        image.put_pixel(0, y, PLACEHOLDER_BORDER);
        image.put_pixel(width - 1, y, PLACEHOLDER_BORDER);
    }
}

fn draw_centered_text(image: &mut RgbImage, lines: &[String]) {
    let rendered_lines = lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| render_text_line(line.trim()))
        .collect::<Vec<RgbImage>>();
    if rendered_lines.is_empty() {
        return;
    }
    let total_height = rendered_lines.iter().map(|line| line.height()).sum::<u32>()
        + (rendered_lines.len().saturating_sub(1) as u32 * 6);
    let mut y = image.height().saturating_sub(total_height) / 2;
    for rendered in rendered_lines {
        let x = image.width().saturating_sub(rendered.width()) / 2;
        overlay(image, &rendered, x.into(), y.into());
        y += rendered.height() + 6;
    }
}

fn render_text_line(text: &str) -> RgbImage {
    let width = text.chars().count().max(1) as u32 * 8;
    let mut image = ImageBuffer::from_pixel(width, 8, PLACEHOLDER_BACKGROUND);
    for (index, ch) in text.chars().enumerate() {
        draw_ascii_char(&mut image, index as u32 * 8, ch);
    }
    image
}

fn draw_ascii_char(image: &mut RgbImage, x_offset: u32, ch: char) {
    let glyph = glyph(ch);
    for (row, mask) in glyph.iter().enumerate() {
        for col in 0..5 {
            if (mask >> (4 - col)) & 1 == 1 {
                image.put_pixel(x_offset + col + 1, row as u32, PLACEHOLDER_TEXT);
            }
        }
    }
}

fn glyph(ch: char) -> [u8; 7] {
    match ch.to_ascii_uppercase() {
        'A' => [
            0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'D' => [
            0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110,
        ],
        'E' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111,
        ],
        'G' => [
            0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01110,
        ],
        'H' => [
            0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'I' => [
            0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b11111,
        ],
        'L' => [
            0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111,
        ],
        'M' => [
            0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001,
        ],
        'N' => [
            0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001, 0b10001,
        ],
        'O' => [
            0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'P' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'R' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001,
        ],
        'S' => [
            0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        'T' => [
            0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        'U' => [
            0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'W' => [
            0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b10101, 0b01010,
        ],
        '/' => [
            0b00001, 0b00010, 0b00010, 0b00100, 0b01000, 0b01000, 0b10000,
        ],
        ' ' => [0, 0, 0, 0, 0, 0, 0],
        _ => [0b11111, 0b10001, 0b00010, 0b00100, 0b00100, 0, 0b00100],
    }
}

#[cfg(test)]
mod tests {
    use super::serialize_office_image;

    #[test]
    fn vector_images_become_hashed_jpg_placeholders() {
        let image = serialize_office_image("image1.emf", None, b"emf-bytes")
            .expect("serialize")
            .expect("placeholder");

        assert!(image.file_name.ends_with(".jpg"));
        assert!(!image.file_name.contains("image1"));
        assert!(image.warning.expect("warning").contains("EMF"));
    }
}
