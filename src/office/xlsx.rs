use std::{fs::File, io::BufReader};

use calamine::{open_workbook, Data, Reader, SheetVisible, Xlsx};
use serde_json::json;

use crate::{
    domain::models::{ParseTask, ParsedDocument, StoredUpload},
    error::{ApiError, ApiResult},
    office::{
        markdown::to_parsed_document,
        model::{OfficeBlock, OfficeDocument, OfficePage},
        writer_adapter::OfficeMediaWriter,
    },
};

pub async fn parse_xlsx(task: &ParseTask, upload: &StoredUpload) -> ApiResult<ParsedDocument> {
    let mut workbook: Xlsx<BufReader<File>> =
        open_workbook::<Xlsx<BufReader<File>>, _>(&upload.path)
            .map_err(|error| ApiError::BadRequest(error.to_string()))?;
    let media_writer = OfficeMediaWriter::new(task, &upload.stem).await?;
    let mut images = Vec::new();
    if let Some(pictures) = workbook.pictures() {
        for (index, (extension, bytes)) in pictures.into_iter().enumerate() {
            let extension = if extension.trim().is_empty() {
                "bin"
            } else {
                extension.trim()
            };
            let file_name = format!("xlsx_image_{}.{}", index + 1, extension);
            match media_writer.write_image(&file_name, &bytes).await {
                Ok(image) => images.push(image),
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
        let mut blocks = blocks_from_sheet(&sheet_name, &range);
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
        pages.push(OfficePage { page_idx, blocks });
    }
    if pages.is_empty() && !images.is_empty() {
        pages.push(OfficePage {
            page_idx: 0,
            blocks: images
                .iter()
                .map(|image| OfficeBlock::Image {
                    path: image.file_name.clone(),
                    alt: "image".to_string(),
                })
                .collect(),
        });
    } else if !images.is_empty() {
        for image in &images {
            if let Some(page) = pages.first_mut() {
                page.blocks.push(OfficeBlock::Image {
                    path: image.file_name.clone(),
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
            "source": upload.stem
        }),
    };
    Ok(to_parsed_document(upload.stem.clone(), office_document))
}

fn blocks_from_sheet(sheet_name: &str, range: &calamine::Range<Data>) -> Vec<OfficeBlock> {
    let used_rows = range
        .rows()
        .map(|row| row.iter().map(data_to_text).collect::<Vec<String>>())
        .collect::<Vec<Vec<String>>>();
    let trimmed = trim_empty_edges(used_rows);
    if trimmed.is_empty() {
        return Vec::new();
    }
    let non_empty_cells = trimmed
        .iter()
        .flat_map(|row| row.iter())
        .filter(|cell| !cell.trim().is_empty())
        .count();
    if non_empty_cells == 1 {
        let text = trimmed
            .iter()
            .flat_map(|row| row.iter())
            .find(|cell| !cell.trim().is_empty())
            .cloned()
            .unwrap_or_default();
        return vec![OfficeBlock::Text { content: text }];
    }
    vec![OfficeBlock::Table {
        html: rows_to_html_table(sheet_name, &trimmed),
    }]
}

fn trim_empty_edges(rows: Vec<Vec<String>>) -> Vec<Vec<String>> {
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
        return Vec::new();
    }
    rows[min_row..=max_row]
        .iter()
        .map(|row| {
            (min_col..=max_col)
                .map(|index| row.get(index).cloned().unwrap_or_default())
                .collect::<Vec<String>>()
        })
        .collect()
}

fn rows_to_html_table(sheet_name: &str, rows: &[Vec<String>]) -> String {
    let caption = format!("<caption>{}</caption>", html_escape(sheet_name));
    let body = rows
        .iter()
        .map(|row| {
            let cells = row
                .iter()
                .map(|cell| format!("<td>{}</td>", html_escape(cell)))
                .collect::<String>();
            format!("<tr>{cells}</tr>")
        })
        .collect::<String>();
    format!("<table>{caption}{body}</table>")
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
