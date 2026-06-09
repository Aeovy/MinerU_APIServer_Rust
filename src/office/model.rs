use std::path::PathBuf;

use serde_json::{json, Value};

#[derive(Debug, Clone, Default)]
pub struct OfficeDocument {
    pub pages: Vec<OfficePage>,
    pub images: Vec<OfficeImage>,
    pub model_output: Value,
}

#[derive(Debug, Clone, Default)]
pub struct OfficePage {
    pub page_idx: usize,
    pub blocks: Vec<OfficeBlock>,
}

#[derive(Debug, Clone)]
pub enum OfficeBlock {
    Text { content: String },
    Title { content: String, level: usize },
    Table { html: String },
    Image { path: String, alt: String },
    Chart { html: String },
    Equation { latex: String },
    List { items: Vec<String> },
}

#[derive(Debug, Clone)]
pub struct OfficeImage {
    pub file_name: String,
    pub source_path: PathBuf,
}

impl OfficeBlock {
    pub fn to_middle_json(&self, index: usize) -> Value {
        match self {
            Self::Text { content } => json!({
                "type": "text",
                "index": index,
                "lines": [line_with_text_span(content)]
            }),
            Self::Title { content, level } => json!({
                "type": "title",
                "index": index,
                "level": level,
                "lines": [line_with_text_span(content)]
            }),
            Self::Table { html } => json!({
                "type": "table",
                "index": index,
                "blocks": [{
                    "type": "table_body",
                    "lines": [{
                        "spans": [{
                            "type": "table",
                            "html": html,
                            "content": html
                        }]
                    }]
                }]
            }),
            Self::Image { path, alt } => json!({
                "type": "image",
                "index": index,
                "blocks": [{
                    "type": "image_body",
                    "lines": [{
                        "spans": [{
                            "type": "image",
                            "image_path": path,
                            "content": alt
                        }]
                    }]
                }]
            }),
            Self::Chart { html } => json!({
                "type": "chart",
                "index": index,
                "blocks": [{
                    "type": "chart_body",
                    "lines": [{
                        "spans": [{
                            "type": "chart",
                            "html": html,
                            "content": html
                        }]
                    }]
                }]
            }),
            Self::Equation { latex } => json!({
                "type": "interline_equation",
                "index": index,
                "lines": [{
                    "spans": [{
                        "type": "interline_equation",
                        "content": latex
                    }]
                }]
            }),
            Self::List { items } => json!({
                "type": "list",
                "index": index,
                "blocks": items
                    .iter()
                    .enumerate()
                    .map(|(item_index, item)| json!({
                        "type": "text",
                        "index": item_index,
                        "lines": [line_with_text_span(item)]
                    }))
                    .collect::<Vec<Value>>()
            }),
        }
    }
}

fn line_with_text_span(content: &str) -> Value {
    json!({
        "spans": [{
            "type": "text",
            "content": content
        }]
    })
}
