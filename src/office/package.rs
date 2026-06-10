use std::{collections::HashMap, fs::File, io::Read, path::Path};

use zip::ZipArchive;

use quick_xml::events::Event;

use crate::error::{ApiError, ApiResult};

const MAX_OOXML_ENTRY_BYTES: u64 = 64 * 1024 * 1024;
const MAX_OOXML_TOTAL_BYTES: u64 = 512 * 1024 * 1024;

pub struct OoxmlPackage {
    files: HashMap<String, Vec<u8>>,
    content_types: HashMap<String, String>,
    default_content_types: HashMap<String, String>,
}

impl OoxmlPackage {
    /// Load an OOXML zip package into bounded in-memory entries.
    ///
    /// Inputs:
    /// - `path`: path to an uploaded docx, pptx, or xlsx file.
    pub fn open(path: &Path) -> ApiResult<Self> {
        let file = File::open(path).map_err(ApiError::from)?;
        let mut archive =
            ZipArchive::new(file).map_err(|error| ApiError::BadRequest(error.to_string()))?;
        let mut files = HashMap::new();
        let mut total_size = 0_u64;
        for index in 0..archive.len() {
            let mut entry = archive
                .by_index(index)
                .map_err(|error| ApiError::BadRequest(error.to_string()))?;
            if entry.is_dir() {
                continue;
            }
            let entry_size = entry.size();
            if entry_size > MAX_OOXML_ENTRY_BYTES {
                return Err(ApiError::BadRequest(format!(
                    "OOXML entry is too large: {}",
                    entry.name()
                )));
            }
            total_size = total_size.saturating_add(entry_size);
            if total_size > MAX_OOXML_TOTAL_BYTES {
                return Err(ApiError::BadRequest(
                    "OOXML package expands beyond the configured safety limit".to_string(),
                ));
            }
            let mut bytes = Vec::with_capacity(entry_size.min(usize::MAX as u64) as usize);
            entry.read_to_end(&mut bytes).map_err(ApiError::from)?;
            files.insert(normalize_part_name(entry.name()), bytes);
        }
        if !files.contains_key("[Content_Types].xml") {
            return Err(ApiError::BadRequest(
                "Invalid OOXML package: missing [Content_Types].xml".to_string(),
            ));
        }
        let (content_types, default_content_types) =
            parse_content_types(files.get("[Content_Types].xml").expect("checked above"))?;
        Ok(Self {
            files,
            content_types,
            default_content_types,
        })
    }

    pub fn read_text(&self, name: &str) -> ApiResult<String> {
        let bytes = self
            .read(name)
            .ok_or_else(|| ApiError::BadRequest(format!("Missing OOXML part: {name}")))?;
        String::from_utf8(bytes.to_vec())
            .map_err(|error| ApiError::BadRequest(format!("Invalid UTF-8 in {name}: {error}")))
    }

    pub fn read(&self, name: &str) -> Option<&[u8]> {
        self.files
            .get(&normalize_part_name(name))
            .map(Vec::as_slice)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.files.contains_key(&normalize_part_name(name))
    }

    pub fn part_names(&self) -> impl Iterator<Item = &str> {
        self.files.keys().map(String::as_str)
    }

    pub fn content_type(&self, name: &str) -> Option<&str> {
        let normalized = normalize_part_name(name);
        self.content_types
            .get(&normalized)
            .or_else(|| {
                Path::new(&normalized)
                    .extension()
                    .and_then(|value| value.to_str())
                    .and_then(|extension| self.default_content_types.get(extension))
            })
            .map(String::as_str)
    }
}

pub fn normalize_part_name(name: &str) -> String {
    name.trim_start_matches('/').replace('\\', "/")
}

pub fn local_name(name: &[u8]) -> &[u8] {
    name.rsplit(|byte| *byte == b':').next().unwrap_or(name)
}

pub fn resolve_part_path(base_part: &str, target: &str) -> String {
    if target.starts_with('/') {
        return normalize_part_name(target);
    }
    let base_dir = Path::new(base_part)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    normalize_posix_path(&base_dir.join(target))
}

pub fn rels_path_for_part(part_name: &str) -> String {
    let path = Path::new(part_name);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(part_name);
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    normalize_posix_path(&parent.join("_rels").join(format!("{file_name}.rels")))
}

fn normalize_posix_path(path: &Path) -> String {
    let mut parts = Vec::new();
    for component in path.components() {
        let value = component.as_os_str().to_string_lossy();
        if value == "." || value.is_empty() {
            continue;
        }
        if value == ".." {
            parts.pop();
            continue;
        }
        parts.push(value.to_string());
    }
    parts.join("/")
}

fn parse_content_types(
    bytes: &[u8],
) -> ApiResult<(HashMap<String, String>, HashMap<String, String>)> {
    let mut reader = quick_xml::Reader::from_reader(std::io::Cursor::new(bytes));
    reader.config_mut().trim_text(true);
    let mut buffer = Vec::new();
    let mut overrides = HashMap::new();
    let mut defaults = HashMap::new();
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(event)) | Ok(Event::Empty(event)) => {
                match local_name(event.name().as_ref()) {
                    b"Override" => {
                        let mut part_name = None;
                        let mut content_type = None;
                        for attr in event.attributes().flatten() {
                            let value = attr
                                .decoded_and_normalized_value(
                                    quick_xml::XmlVersion::Implicit1_0,
                                    reader.decoder(),
                                )
                                .map_err(|error| ApiError::BadRequest(error.to_string()))?
                                .into_owned();
                            match local_name(attr.key.as_ref()) {
                                b"PartName" => part_name = Some(normalize_part_name(&value)),
                                b"ContentType" => content_type = Some(value),
                                _ => {}
                            }
                        }
                        if let (Some(part_name), Some(content_type)) = (part_name, content_type) {
                            overrides.insert(part_name, content_type);
                        }
                    }
                    b"Default" => {
                        let mut extension = None;
                        let mut content_type = None;
                        for attr in event.attributes().flatten() {
                            let value = attr
                                .decoded_and_normalized_value(
                                    quick_xml::XmlVersion::Implicit1_0,
                                    reader.decoder(),
                                )
                                .map_err(|error| ApiError::BadRequest(error.to_string()))?
                                .into_owned();
                            match local_name(attr.key.as_ref()) {
                                b"Extension" => extension = Some(value.to_ascii_lowercase()),
                                b"ContentType" => content_type = Some(value),
                                _ => {}
                            }
                        }
                        if let (Some(extension), Some(content_type)) = (extension, content_type) {
                            defaults.insert(extension, content_type);
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(error) => return Err(ApiError::BadRequest(error.to_string())),
        }
        buffer.clear();
    }
    Ok((overrides, defaults))
}

#[cfg(test)]
mod tests {
    use super::{normalize_part_name, rels_path_for_part, resolve_part_path};

    #[test]
    fn resolves_relative_relationship_targets() {
        assert_eq!(
            resolve_part_path("word/document.xml", "media/image1.png"),
            "word/media/image1.png"
        );
        assert_eq!(
            resolve_part_path("ppt/slides/slide1.xml", "../media/image1.png"),
            "ppt/media/image1.png"
        );
        assert_eq!(
            resolve_part_path("word/document.xml", "/docProps/core.xml"),
            "docProps/core.xml"
        );
    }

    #[test]
    fn builds_relationship_part_path() {
        assert_eq!(
            rels_path_for_part("ppt/slides/slide1.xml"),
            "ppt/slides/_rels/slide1.xml.rels"
        );
    }

    #[test]
    fn normalizes_part_names() {
        assert_eq!(
            normalize_part_name("/word\\document.xml"),
            "word/document.xml"
        );
    }
}
