use std::{fs::File, io::BufReader};

use calamine::{open_workbook, Data, Dimensions, Reader, SheetVisible, Xlsx};
use serde_json::json;

use crate::{
    domain::models::{ParseTask, ParsedDocument, StoredUpload},
    error::{ApiError, ApiResult},
    office::{
        markdown::to_parsed_document,
        model::{OfficeBlock, OfficeDocument, OfficePage},
        writer_adapter::{OfficeImageWrite, OfficeMediaWriter},
    },
};

pub async fn parse_xlsx(task: &ParseTask, upload: &StoredUpload) -> ApiResult<ParsedDocument> {
    let mut workbook: Xlsx<BufReader<File>> =
        open_workbook::<Xlsx<BufReader<File>>, _>(&upload.path)
            .map_err(|error| ApiError::BadRequest(error.to_string()))?;
    let media_writer = OfficeMediaWriter::new(task, &upload.stem).await?;
    let mut images = Vec::new();
    let mut warnings = Vec::new();
    if let Some(pictures) = workbook.pictures() {
        for (index, (extension, bytes)) in pictures.into_iter().enumerate() {
            let extension = if extension.trim().is_empty() {
                "bin"
            } else {
                extension.trim()
            };
            let file_name = format!("xlsx_image_{}.{}", index + 1, extension);
            match media_writer.write_image(&file_name, None, &bytes).await {
                Ok(OfficeImageWrite::Written { image, warning }) => {
                    if let Some(warning) = warning {
                        warnings.push(warning);
                    }
                    images.push(image);
                }
                Ok(OfficeImageWrite::Skipped { reason, detail }) => {
                    warnings.push(format!("{reason}: {detail}"));
                    tracing::warn!(reason, detail, "skipped XLSX image");
                }
                Err(error) => {
                    tracing::warn!(error = %error.detail(), "failed to write XLSX image");
                }
            }
        }
    }

    let visible_sheets = workbook
        .sheets_metadata()
        .iter()
        .filter(|sheet| sheet.visible == SheetVisible::Visible)
        .map(|sheet| sheet.name.clone())
        .collect::<Vec<String>>();
    let mut sheet_pages: Vec<(String, Vec<OfficeBlock>)> = Vec::new();
    for sheet_name in visible_sheets {
        let range = workbook
            .worksheet_range(&sheet_name)
            .map_err(|error| ApiError::BadRequest(error.to_string()))?;
        let merges = workbook
            .worksheet_merge_cells(&sheet_name)
            .transpose()
            .map_err(|error| ApiError::BadRequest(error.to_string()))?
            .unwrap_or_default();
        let mut blocks = blocks_from_sheet(&sheet_name, &range, &merges);
        if blocks.is_empty() {
            continue;
        }
        sheet_pages.push((sheet_name, std::mem::take(&mut blocks)));
    }

    let should_emit_sheet_titles = sheet_pages.len() > 1;
    let mut pages = Vec::new();
    for (page_idx, (sheet_name, mut blocks)) in sheet_pages.into_iter().enumerate() {
        if should_emit_sheet_titles {
            blocks.insert(
                0,
                OfficeBlock::Title {
                    content: sheet_name,
                    level: 1,
                },
            );
        }
        pages.push(OfficePage {
            page_idx,
            blocks,
            discarded_blocks: Vec::new(),
        });
    }
    if pages.is_empty() && !images.is_empty() {
        pages.push(OfficePage {
            page_idx: 0,
            blocks: images
                .iter()
                .map(|image| OfficeBlock::Image {
                    path: image.display_path.clone(),
                    alt: "image".to_string(),
                })
                .collect(),
            discarded_blocks: Vec::new(),
        });
    } else if !images.is_empty() {
        for image in &images {
            if let Some(page) = pages.first_mut() {
                page.blocks.push(OfficeBlock::Image {
                    path: image.display_path.clone(),
                    alt: "image".to_string(),
                });
            }
        }
    }

    let office_document = OfficeDocument {
        pages,
        images,
        model_output: json!({
            "type": "xlsx",
            "source": upload.stem,
            "warnings": warnings
        }),
    };
    Ok(to_parsed_document(upload.stem.clone(), office_document))
}

fn blocks_from_sheet(
    sheet_name: &str,
    range: &calamine::Range<Data>,
    merges: &[Dimensions],
) -> Vec<OfficeBlock> {
    let used_rows = range
        .rows()
        .map(|row| row.iter().map(data_to_text).collect::<Vec<String>>())
        .collect::<Vec<Vec<String>>>();
    let trimmed = trim_empty_edges(used_rows);
    if trimmed.is_empty() {
        return Vec::new();
    }
    let non_empty_cells = trimmed
        .rows
        .iter()
        .flat_map(|row| row.iter())
        .filter(|cell| !cell.trim().is_empty())
        .count();
    if non_empty_cells == 1 {
        let text = trimmed
            .rows
            .iter()
            .flat_map(|row| row.iter())
            .find(|cell| !cell.trim().is_empty())
            .cloned()
            .unwrap_or_default();
        return vec![OfficeBlock::Text { content: text }];
    }
    vec![OfficeBlock::Table {
        html: rows_to_html_table(sheet_name, &trimmed.rows, &trimmed.merges_from(merges)),
    }]
}

#[derive(Debug, Clone)]
struct TrimmedSheet {
    rows: Vec<Vec<String>>,
    row_offset: usize,
    col_offset: usize,
}

impl TrimmedSheet {
    fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Convert absolute workbook merge dimensions to table-relative dimensions.
    ///
    /// Inputs:
    /// - `merges`: merge dimensions reported by calamine in zero-based sheet coordinates.
    fn merges_from(&self, merges: &[Dimensions]) -> Vec<Dimensions> {
        let row_offset = self.row_offset as u32;
        let col_offset = self.col_offset as u32;
        let row_count = self.rows.len() as u32;
        let col_count = self.rows.first().map(Vec::len).unwrap_or_default() as u32;
        merges
            .iter()
            .filter_map(|merge| {
                if merge.start.0 < row_offset || merge.start.1 < col_offset {
                    return None;
                }
                let start_row = merge.start.0 - row_offset;
                let start_col = merge.start.1 - col_offset;
                let end_row = merge.end.0.saturating_sub(row_offset);
                let end_col = merge.end.1.saturating_sub(col_offset);
                if start_row >= row_count || start_col >= col_count {
                    return None;
                }
                Some(Dimensions {
                    start: (start_row, start_col),
                    end: (
                        end_row.min(row_count.saturating_sub(1)),
                        end_col.min(col_count.saturating_sub(1)),
                    ),
                })
            })
            .filter(|merge| merge.end.0 > merge.start.0 || merge.end.1 > merge.start.1)
            .collect()
    }
}

fn trim_empty_edges(rows: Vec<Vec<String>>) -> TrimmedSheet {
    let mut min_row = usize::MAX;
    let mut max_row = 0_usize;
    let mut min_col = usize::MAX;
    let mut max_col = 0_usize;
    for (row_index, row) in rows.iter().enumerate() {
        for (col_index, cell) in row.iter().enumerate() {
            if cell.trim().is_empty() {
                continue;
            }
            min_row = min_row.min(row_index);
            max_row = max_row.max(row_index);
            min_col = min_col.min(col_index);
            max_col = max_col.max(col_index);
        }
    }
    if min_row == usize::MAX {
        return TrimmedSheet {
            rows: Vec::new(),
            row_offset: 0,
            col_offset: 0,
        };
    }
    let rows = rows[min_row..=max_row]
        .iter()
        .map(|row| {
            (min_col..=max_col)
                .map(|index| row.get(index).cloned().unwrap_or_default())
                .collect::<Vec<String>>()
        })
        .collect();
    TrimmedSheet {
        rows,
        row_offset: min_row,
        col_offset: min_col,
    }
}

fn rows_to_html_table(sheet_name: &str, rows: &[Vec<String>], merges: &[Dimensions]) -> String {
    let caption = format!("<caption>{}</caption>", html_escape(sheet_name));
    let body = rows
        .iter()
        .enumerate()
        .map(|(row_index, row)| {
            let cells = row
                .iter()
                .enumerate()
                .filter_map(|(col_index, cell)| {
                    cell_to_html(row_index as u32, col_index as u32, cell, merges)
                })
                .collect::<String>();
            format!("<tr>{cells}</tr>")
        })
        .collect::<String>();
    format!("<table>{caption}{body}</table>")
}

fn cell_to_html(row: u32, col: u32, cell: &str, merges: &[Dimensions]) -> Option<String> {
    if merges
        .iter()
        .any(|merge| merge.contains(row, col) && merge.start != (row, col))
    {
        return None;
    }
    let mut attrs = String::new();
    if let Some(merge) = merges.iter().find(|merge| merge.start == (row, col)) {
        let rowspan = merge.end.0 - merge.start.0 + 1;
        let colspan = merge.end.1 - merge.start.1 + 1;
        if rowspan > 1 {
            attrs.push_str(&format!(r#" rowspan="{rowspan}""#));
        }
        if colspan > 1 {
            attrs.push_str(&format!(r#" colspan="{colspan}""#));
        }
    }
    Some(format!("<td{attrs}>{}</td>", html_escape(cell)))
}

fn data_to_text(cell: &Data) -> String {
    match cell {
        Data::Empty => String::new(),
        Data::String(value) => value.clone(),
        Data::Float(value) => {
            if value.fract() == 0.0 {
                format!("{value:.0}")
            } else {
                value.to_string()
            }
        }
        Data::Int(value) => value.to_string(),
        Data::Bool(value) => value.to_string(),
        Data::DateTime(value) => value.to_string(),
        Data::DateTimeIso(value) | Data::DurationIso(value) => value.clone(),
        Data::Error(value) => value.to_string(),
    }
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
