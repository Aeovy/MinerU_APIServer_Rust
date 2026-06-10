use std::sync::OnceLock;

use regex::Regex;
use serde_json::{json, Value};

use crate::{
    config::MINERU_VERSION,
    domain::models::ParsedDocument,
    office::{
        inline::{parse_inline_spans, InlineSpan},
        model::{OfficeBlock, OfficeDocument},
    },
};

pub fn to_parsed_document(file_name: String, office_document: OfficeDocument) -> ParsedDocument {
    let markdown = make_markdown(&office_document);
    let middle_json = make_middle_json(&office_document);
    let content_list = make_content_list(&office_document);
    let content_list_v2 = make_content_list_v2(&office_document);
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
                "discarded_blocks": page
                    .discarded_blocks
                    .iter()
                    .map(|block| block.to_middle_json())
                    .collect::<Vec<Value>>()
            }))
            .collect::<Vec<Value>>(),
        "_backend": "office",
        "_version_name": MINERU_VERSION
    })
}

fn make_content_list(document: &OfficeDocument) -> Value {
    let mut items = Vec::new();
    for page in &document.pages {
        for block in &page.blocks {
            let mut item = match block {
                OfficeBlock::Text { content } => json!({
                    "type": "text",
                    "text": render_inline_markdown(content)
                }),
                OfficeBlock::Title { content, level } => json!({
                    "type": "text",
                    "text": render_inline_markdown(content),
                    "text_level": level
                }),
                OfficeBlock::Table { html } => json!({
                    "type": "table",
                    "table_caption": [],
                    "table_body": format_embedded_html(html)
                }),
                OfficeBlock::Image { path, alt } => json!({
                    "type": "image",
                    "img_path": path,
                    "image_path": path,
                    "image_caption": [],
                    "image_footnote": [],
                    "content": alt
                }),
                OfficeBlock::Chart { html } => json!({
                    "type": "chart",
                    "img_path": "",
                    "content": format_embedded_html(html),
                    "chart_caption": [],
                    "chart_footnote": []
                }),
                OfficeBlock::Equation { latex } => json!({
                    "type": "equation",
                    "text": latex,
                    "text_format": "latex"
                }),
                OfficeBlock::List { items } => json!({
                    "type": "list",
                    "list_items": items
                        .iter()
                        .map(|item| render_inline_markdown(item))
                        .filter(|item| !item.trim().is_empty())
                        .collect::<Vec<String>>()
                }),
            };
            if let Some(object) = item.as_object_mut() {
                object.insert("page_idx".to_string(), json!(page.page_idx));
            }
            items.push(item);
        }
    }
    Value::Array(items)
}

/// Build the Office-flavored content-list v2 shape used by MinerU.
///
/// Inputs:
/// - `document`: normalized Office block IR grouped by page.
fn make_content_list_v2(document: &OfficeDocument) -> Value {
    Value::Array(
        document
            .pages
            .iter()
            .map(|page| {
                Value::Array(
                    page.blocks
                        .iter()
                        .map(make_block_content_v2)
                        .collect::<Vec<Value>>(),
                )
            })
            .collect::<Vec<Value>>(),
    )
}

fn make_block_content_v2(block: &OfficeBlock) -> Value {
    match block {
        OfficeBlock::Text { content } => json!({
            "type": "paragraph",
            "content": {"paragraph_content": inline_content_v2(content)}
        }),
        OfficeBlock::Title { content, level } => json!({
            "type": "title",
            "content": {
                "title_content": inline_content_v2(content),
                "level": level
            }
        }),
        OfficeBlock::Table { html } => table_content_v2(html),
        OfficeBlock::Image { path, .. } => json!({
            "type": "image",
            "content": {
                "image_source": {"path": path},
                "image_caption": []
            }
        }),
        OfficeBlock::Chart { html } => json!({
            "type": "chart",
            "content": {
                "image_source": {"path": ""},
                "content": format_embedded_html(html),
                "chart_caption": []
            }
        }),
        OfficeBlock::Equation { latex } => json!({
            "type": "equation_interline",
            "content": {
                "math_content": latex,
                "math_type": "latex"
            }
        }),
        OfficeBlock::List { items } => json!({
            "type": "list",
            "content": {
                "list_type": "text_list",
                "attribute": "unordered",
                "list_items": items
                    .iter()
                    .filter_map(|item| make_list_item_v2(item))
                    .collect::<Vec<Value>>()
            }
        }),
    }
}

fn table_content_v2(html: &str) -> Value {
    let table_html = format_embedded_html(html);
    let nest_level = if table_html.matches("<table").count() > 1 {
        2
    } else {
        1
    };
    let table_type =
        if table_html.contains("colspan") || table_html.contains("rowspan") || nest_level > 1 {
            "complex_table"
        } else {
            "simple_table"
        };
    json!({
        "type": "table",
        "content": {
            "table_caption": [],
            "html": table_html,
            "table_type": table_type,
            "table_nest_level": nest_level
        }
    })
}

fn make_list_item_v2(item: &str) -> Option<Value> {
    let item_content = inline_content_v2(item);
    if item_content.is_empty() {
        return None;
    }
    Some(json!({
        "item_type": "text",
        "ilevel": 0,
        "prefix": "-",
        "item_content": item_content
    }))
}

fn inline_content_v2(content: &str) -> Vec<Value> {
    parse_inline_spans(content)
        .into_iter()
        .filter_map(|span| match span {
            InlineSpan::Text { content, styles } => {
                if content.trim().is_empty()
                    && !styles
                        .iter()
                        .any(|style| matches!(style.as_str(), "underline" | "strikethrough"))
                {
                    return None;
                }
                let mut value = json!({
                    "type": "text",
                    "content": content
                });
                if !styles.is_empty() {
                    value["style"] = json!(styles);
                }
                Some(value)
            }
            InlineSpan::InlineEquation { content } => {
                if content.trim().is_empty() {
                    None
                } else {
                    Some(json!({
                        "type": "equation_inline",
                        "content": content
                    }))
                }
            }
            InlineSpan::Hyperlink {
                content,
                url,
                styles,
            } => {
                if content.trim().is_empty() {
                    return None;
                }
                let mut value = json!({
                    "type": "hyperlink",
                    "content": content,
                    "url": url
                });
                if !styles.is_empty() {
                    value["style"] = json!(styles);
                }
                Some(value)
            }
        })
        .collect()
}

fn make_markdown(document: &OfficeDocument) -> String {
    let mut parts = Vec::new();
    for page in &document.pages {
        for block in &page.blocks {
            match block {
                OfficeBlock::Text { content } => parts.push(render_inline_markdown(content)),
                OfficeBlock::Title { content, level } => {
                    let depth = (*level).clamp(1, 6);
                    parts.push(format!(
                        "{} {}",
                        "#".repeat(depth),
                        render_inline_markdown(content)
                    ));
                }
                OfficeBlock::Table { html } | OfficeBlock::Chart { html } => {
                    parts.push(format_embedded_html(html));
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
                            .map(|item| format!("- {}", render_inline_markdown(item)))
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

fn format_embedded_html(html: &str) -> String {
    let equation_replaced = replace_equation_tags(html);
    // Office parsers already emit image paths as `images/...`; table HTML may
    // still contain local bare filenames from embedded objects.
    image_src_pattern()
        .replace_all(&equation_replaced, |captures: &regex::Captures<'_>| {
            let source = captures.get(1).map_or("", |source| source.as_str());
            if source.starts_with("data:")
                || source.starts_with("http://")
                || source.starts_with("https://")
                || source.starts_with("images/")
            {
                captures
                    .get(0)
                    .map_or_else(String::new, |value| value.as_str().to_string())
            } else {
                format!(r#"src="images/{source}""#)
            }
        })
        .into_owned()
}

fn image_src_pattern() -> &'static Regex {
    static IMAGE_SRC_PATTERN: OnceLock<Regex> = OnceLock::new();
    IMAGE_SRC_PATTERN.get_or_init(|| Regex::new(r#"src="([^"]+)""#).expect("valid image src regex"))
}

fn render_inline_markdown(content: &str) -> String {
    parse_inline_spans(content)
        .iter()
        .map(render_inline_span)
        .collect::<String>()
}

fn render_inline_span(span: &InlineSpan) -> String {
    match span {
        InlineSpan::Text { content, styles } => apply_markdown_styles(content, styles),
        InlineSpan::InlineEquation { content } => format!("${}$", content.trim()),
        InlineSpan::Hyperlink {
            content,
            url,
            styles,
        } => format!(
            "[{}]({})",
            apply_markdown_styles(content, styles),
            escape_link_destination(url)
        ),
    }
}

fn apply_markdown_styles(content: &str, styles: &[String]) -> String {
    let mut rendered = escape_markdown_text(content);
    if styles.iter().any(|style| style == "bold") {
        rendered = format!("**{rendered}**");
    }
    if styles.iter().any(|style| style == "italic") {
        rendered = format!("*{rendered}*");
    }
    if styles.iter().any(|style| style == "underline") {
        rendered = format!("<u>{rendered}</u>");
    }
    if styles.iter().any(|style| style == "strikethrough") {
        rendered = format!("~~{rendered}~~");
    }
    rendered
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

fn escape_link_destination(text: &str) -> String {
    text.replace(')', "%29").replace(' ', "%20")
}
