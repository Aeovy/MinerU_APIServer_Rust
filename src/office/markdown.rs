use serde_json::{json, Value};

use crate::{
    config::MINERU_VERSION,
    domain::models::ParsedDocument,
    office::model::{OfficeBlock, OfficeDocument},
};

pub fn to_parsed_document(file_name: String, office_document: OfficeDocument) -> ParsedDocument {
    let markdown = make_markdown(&office_document);
    let middle_json = make_middle_json(&office_document);
    let content_list = make_content_list(&office_document, false);
    let content_list_v2 = make_content_list(&office_document, true);
    let image_files = office_document
        .images
        .into_iter()
        .map(|image| image.source_path)
        .collect();

    ParsedDocument {
        file_name,
        markdown,
        middle_json,
        model_output: office_document.model_output,
        content_list,
        content_list_v2,
        image_files,
    }
}

fn make_middle_json(document: &OfficeDocument) -> Value {
    json!({
        "pdf_info": document
            .pages
            .iter()
            .map(|page| json!({
                "page_idx": page.page_idx,
                "para_blocks": page
                    .blocks
                    .iter()
                    .enumerate()
                    .map(|(index, block)| block.to_middle_json(index))
                    .collect::<Vec<Value>>(),
                "discarded_blocks": []
            }))
            .collect::<Vec<Value>>(),
        "_backend": "office",
        "_version_name": MINERU_VERSION
    })
}

fn make_content_list(document: &OfficeDocument, include_page: bool) -> Value {
    let mut items = Vec::new();
    for page in &document.pages {
        for block in &page.blocks {
            let mut item = match block {
                OfficeBlock::Text { content } => json!({
                    "type": "text",
                    "text": content
                }),
                OfficeBlock::Title { content, level } => json!({
                    "type": "text",
                    "text": content,
                    "text_level": level
                }),
                OfficeBlock::Table { html } => json!({
                    "type": "table",
                    "table_body": html,
                    "text": html
                }),
                OfficeBlock::Image { path, alt } => json!({
                    "type": "image",
                    "img_path": path,
                    "image_path": path,
                    "image_caption": [],
                    "image_footnote": [],
                    "text": alt
                }),
                OfficeBlock::Chart { html } => json!({
                    "type": "table",
                    "table_body": html,
                    "text": html
                }),
                OfficeBlock::Equation { latex } => json!({
                    "type": "equation",
                    "text": latex
                }),
                OfficeBlock::List { items } => json!({
                    "type": "text",
                    "text": items.join("\n")
                }),
            };
            if include_page {
                if let Some(object) = item.as_object_mut() {
                    object.insert("page_idx".to_string(), json!(page.page_idx));
                }
            }
            items.push(item);
        }
    }
    Value::Array(items)
}

fn make_markdown(document: &OfficeDocument) -> String {
    let mut parts = Vec::new();
    for page in &document.pages {
        for block in &page.blocks {
            match block {
                OfficeBlock::Text { content } => parts.push(escape_markdown_text(content)),
                OfficeBlock::Title { content, level } => {
                    let depth = (*level).clamp(1, 6);
                    parts.push(format!(
                        "{} {}",
                        "#".repeat(depth),
                        escape_markdown_text(content)
                    ));
                }
                OfficeBlock::Table { html } | OfficeBlock::Chart { html } => {
                    parts.push(replace_equation_tags(html));
                }
                OfficeBlock::Image { path, alt } => {
                    parts.push(format!("![{}]({})", escape_link_text(alt), path));
                }
                OfficeBlock::Equation { latex } => {
                    parts.push(format!("$$\n{}\n$$", latex.trim()));
                }
                OfficeBlock::List { items } => {
                    parts.push(
                        items
                            .iter()
                            .map(|item| format!("- {}", escape_markdown_text(item)))
                            .collect::<Vec<String>>()
                            .join("\n"),
                    );
                }
            }
        }
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("{}\n", parts.join("\n\n"))
    }
}

fn replace_equation_tags(html: &str) -> String {
    html.replace("<eq>", " $").replace("</eq>", "$ ")
}

fn escape_markdown_text(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('[', "\\[")
        .replace(']', "\\]")
        .replace('_', "\\_")
}

fn escape_link_text(text: &str) -> String {
    text.replace('[', "\\[").replace(']', "\\]")
}
