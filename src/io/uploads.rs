use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use axum::extract::multipart::Field;
use tokio::{fs, io::AsyncWriteExt};

use crate::{
    domain::models::StoredUpload,
    error::{ApiError, ApiResult},
};

const MAX_TASK_STEM_BYTES: usize = 200;
const SUPPORTED_SUFFIXES: &[&str] = &[
    "pdf", "png", "jpeg", "jp2", "webp", "gif", "bmp", "jpg", "tiff",
];

pub struct UploadStore {
    upload_dir: PathBuf,
}

impl UploadStore {
    pub fn new(upload_dir: PathBuf) -> Self {
        Self { upload_dir }
    }

    /// Persist one multipart upload to the task upload directory.
    ///
    /// Inputs:
    /// - `field`: multipart file field named `files`.
    pub async fn save_field(&self, mut field: Field<'_>) -> ApiResult<StoredUpload> {
        fs::create_dir_all(&self.upload_dir).await?;
        let original_name = field.file_name().map(ToOwned::to_owned).ok_or_else(|| {
            ApiError::BadRequest(
                "Field 'files' must be uploaded as a file. Use curl syntax like -F 'files=@/path/to/document.pdf;type=application/pdf'.".to_string(),
            )
        })?;
        let filename = normalize_upload_filename(&original_name);
        let suffix = file_suffix(&filename)
            .ok_or_else(|| ApiError::BadRequest(format!("Unsupported file type: {filename}")))?;
        if !SUPPORTED_SUFFIXES.contains(&suffix.as_str()) {
            return Err(ApiError::BadRequest(format!(
                "Unsupported file type: {suffix}"
            )));
        }
        let destination = self.unique_destination(&filename).await;
        let mut output = fs::File::create(&destination).await?;
        while let Some(chunk) = field
            .chunk()
            .await
            .map_err(|error| ApiError::BadRequest(error.to_string()))?
        {
            output.write_all(&chunk).await?;
        }
        output.flush().await?;

        Ok(StoredUpload {
            stem: normalize_task_stem(file_stem(&filename).unwrap_or_default()),
            path: destination,
            suffix,
        })
    }

    async fn unique_destination(&self, filename: &str) -> PathBuf {
        let destination = self.upload_dir.join(filename);
        if !destination.exists() {
            return destination;
        }
        let base = file_stem(filename).unwrap_or_default();
        let suffix = Path::new(filename)
            .extension()
            .and_then(|value| value.to_str())
            .map(|value| format!(".{value}"))
            .unwrap_or_default();
        for index in 2.. {
            let candidate = self
                .upload_dir
                .join(format!("{base}__upload_{index}{suffix}"));
            if !candidate.exists() {
                return candidate;
            }
        }
        unreachable!("unbounded upload destination search must return")
    }
}

/// Make task stems unique within a request while preserving Python's case-insensitive behavior.
///
/// Inputs:
/// - `uploads`: uploaded files collected in request order.
pub fn uniquify_upload_stems(uploads: &mut [StoredUpload]) {
    let normalized_inputs: Vec<String> = uploads
        .iter()
        .map(|upload| normalize_task_stem(&upload.stem))
        .collect();
    let raw_keys: HashSet<String> = normalized_inputs
        .iter()
        .map(|stem| stem.to_lowercase())
        .collect();
    let mut occurrence_counts: HashMap<String, usize> = HashMap::new();
    let mut assigned_keys: HashSet<String> = HashSet::new();

    for (upload, normalized_stem) in uploads.iter_mut().zip(normalized_inputs) {
        let stem_base = if normalized_stem.is_empty() {
            upload.stem.clone()
        } else {
            normalized_stem
        };
        let stem_key = stem_base.to_lowercase();
        let seen_count = *occurrence_counts.get(&stem_key).unwrap_or(&0);
        occurrence_counts.insert(stem_key, seen_count + 1);

        if seen_count == 0 && !assigned_keys.contains(&stem_base.to_lowercase()) {
            assigned_keys.insert(stem_base.to_lowercase());
            upload.stem = stem_base;
            continue;
        }

        let mut suffix = seen_count + 1;
        loop {
            let candidate = build_task_stem_candidate(&stem_base, &format!("_{suffix}"));
            let candidate_key = candidate.to_lowercase();
            if !raw_keys.contains(&candidate_key) && !assigned_keys.contains(&candidate_key) {
                assigned_keys.insert(candidate_key);
                upload.stem = candidate;
                break;
            }
            suffix += 1;
        }
    }
}

pub fn normalize_upload_filename(upload_name: &str) -> String {
    let sanitized = Path::new(upload_name)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(upload_name);
    let path = Path::new(sanitized);
    let stem = normalize_task_stem(
        path.file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or_default(),
    );
    let suffix = path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| format!(".{value}"))
        .unwrap_or_default();
    format!("{stem}{suffix}")
}

pub fn normalize_task_stem(stem: &str) -> String {
    truncate_to_utf8_bytes(stem, MAX_TASK_STEM_BYTES)
}

fn build_task_stem_candidate(stem: &str, suffix: &str) -> String {
    if utf8_byte_length(&format!("{stem}{suffix}")) <= MAX_TASK_STEM_BYTES {
        return format!("{stem}{suffix}");
    }
    let suffix_bytes = utf8_byte_length(suffix);
    if suffix_bytes >= MAX_TASK_STEM_BYTES {
        return truncate_to_utf8_bytes(suffix, MAX_TASK_STEM_BYTES);
    }
    format!(
        "{}{suffix}",
        truncate_to_utf8_bytes(stem, MAX_TASK_STEM_BYTES - suffix_bytes)
    )
}

fn truncate_to_utf8_bytes(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes.min(value.len());
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

fn utf8_byte_length(value: &str) -> usize {
    value.len()
}

fn file_stem(filename: &str) -> Option<&str> {
    Path::new(filename)
        .file_stem()
        .and_then(|value| value.to_str())
}

fn file_suffix(filename: &str) -> Option<String> {
    Path::new(filename)
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::{normalize_upload_filename, uniquify_upload_stems};
    use crate::domain::models::StoredUpload;
    use std::path::PathBuf;

    #[test]
    fn strips_paths_from_upload_names() {
        assert_eq!(normalize_upload_filename("../dir/report.pdf"), "report.pdf");
    }

    #[test]
    fn uniquifies_duplicate_stems() {
        let mut uploads = vec![
            StoredUpload {
                stem: "a".to_string(),
                path: PathBuf::new(),
                suffix: "pdf".to_string(),
            },
            StoredUpload {
                stem: "A".to_string(),
                path: PathBuf::new(),
                suffix: "pdf".to_string(),
            },
        ];
        uniquify_upload_stems(&mut uploads);
        assert_eq!(uploads[0].stem, "a");
        assert_eq!(uploads[1].stem, "A_2");
    }
}
