use serde_json::{json, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InlineSpan {
    Text {
        content: String,
        styles: Vec<String>,
    },
    InlineEquation {
        content: String,
    },
    Hyperlink {
        content: String,
        url: String,
        styles: Vec<String>,
    },
}

impl InlineSpan {
    pub fn to_middle_json(&self) -> Value {
        match self {
            Self::Text { content, styles } => {
                let mut span = json!({
                    "type": "text",
                    "content": content
                });
                if !styles.is_empty() {
                    span["style"] = json!(styles);
                }
                span
            }
            Self::InlineEquation { content } => json!({
                "type": "inline_equation",
                "content": content
            }),
            Self::Hyperlink {
                content,
                url,
                styles,
            } => {
                let mut span = json!({
                    "type": "hyperlink",
                    "content": content,
                    "url": url
                });
                if !styles.is_empty() {
                    span["style"] = json!(styles);
                }
                span
            }
        }
    }
}

/// Parse the lightweight inline tags emitted by the Office parsers.
///
/// Inputs:
/// - `content`: text content that may contain `<eq>`, `<text style="...">`, or
///   `<hyperlink><text>...</text><url>...</url></hyperlink>` markers.
pub fn parse_inline_spans(content: &str) -> Vec<InlineSpan> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut spans = Vec::new();
    let mut position = 0_usize;
    while position < content.len() {
        let Some((tag_start, tag_type)) = find_next_tag(content, position) else {
            push_plain_text(&mut spans, &content[position..]);
            break;
        };
        if tag_start > position {
            push_plain_text(&mut spans, &content[position..tag_start]);
        }
        match tag_type {
            InlineTag::Equation => {
                if let Some((formula, next_position)) =
                    parse_wrapped_tag(content, tag_start, "<eq>", "</eq>")
                {
                    spans.push(InlineSpan::InlineEquation {
                        content: decode_basic_entities(formula.trim()),
                    });
                    position = next_position;
                } else {
                    push_plain_text(&mut spans, &content[tag_start..]);
                    break;
                }
            }
            InlineTag::Text => {
                if let Some((text, styles, next_position)) = parse_text_tag(content, tag_start) {
                    spans.push(InlineSpan::Text {
                        content: decode_basic_entities(text),
                        styles,
                    });
                    position = next_position;
                } else {
                    push_plain_text(&mut spans, &content[tag_start..]);
                    break;
                }
            }
            InlineTag::Hyperlink => {
                if let Some((text, url, styles, next_position)) =
                    parse_hyperlink_tag(content, tag_start)
                {
                    spans.push(InlineSpan::Hyperlink {
                        content: decode_basic_entities(text),
                        url: decode_basic_entities(url),
                        styles,
                    });
                    position = next_position;
                } else {
                    push_plain_text(&mut spans, &content[tag_start..]);
                    break;
                }
            }
        }
    }
    merge_adjacent_text_spans(spans)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InlineTag {
    Equation,
    Text,
    Hyperlink,
}

fn find_next_tag(content: &str, position: usize) -> Option<(usize, InlineTag)> {
    let candidates = [
        content[position..]
            .find("<eq>")
            .map(|offset| (position + offset, InlineTag::Equation)),
        content[position..]
            .find("<hyperlink>")
            .map(|offset| (position + offset, InlineTag::Hyperlink)),
        content[position..]
            .find("<text")
            .map(|offset| (position + offset, InlineTag::Text)),
    ];
    candidates
        .into_iter()
        .flatten()
        .min_by_key(|(start, _)| *start)
}

fn parse_wrapped_tag<'a>(
    content: &'a str,
    start: usize,
    opening: &str,
    closing: &str,
) -> Option<(&'a str, usize)> {
    if !content[start..].starts_with(opening) {
        return None;
    }
    let value_start = start + opening.len();
    let close_offset = content[value_start..].find(closing)?;
    let value_end = value_start + close_offset;
    Some((&content[value_start..value_end], value_end + closing.len()))
}

fn parse_text_tag(content: &str, start: usize) -> Option<(&str, Vec<String>, usize)> {
    if !content[start..].starts_with("<text") {
        return None;
    }
    let open_end = start + content[start..].find('>')? + 1;
    let opening = &content[start..open_end];
    let close_offset = content[open_end..].find("</text>")?;
    let text_end = open_end + close_offset;
    Some((
        &content[open_end..text_end],
        parse_style_attr(opening),
        text_end + "</text>".len(),
    ))
}

fn parse_hyperlink_tag(content: &str, start: usize) -> Option<(&str, &str, Vec<String>, usize)> {
    let (inner, next_position) = parse_wrapped_tag(content, start, "<hyperlink>", "</hyperlink>")?;
    let (text, styles, _) = parse_text_tag(inner, 0)?;
    let (url, _) = parse_wrapped_tag(inner, inner.find("<url>")?, "<url>", "</url>")?;
    Some((text, url, styles, next_position))
}

fn parse_style_attr(opening_tag: &str) -> Vec<String> {
    let Some(start) = opening_tag.find("style=\"") else {
        return Vec::new();
    };
    let value_start = start + "style=\"".len();
    let Some(value_end) = opening_tag[value_start..].find('"') else {
        return Vec::new();
    };
    opening_tag[value_start..value_start + value_end]
        .split(',')
        .map(str::trim)
        .filter(|style| !style.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn push_plain_text(spans: &mut Vec<InlineSpan>, content: &str) {
    if content.is_empty() {
        return;
    }
    spans.push(InlineSpan::Text {
        content: decode_basic_entities(content),
        styles: Vec::new(),
    });
}

fn merge_adjacent_text_spans(spans: Vec<InlineSpan>) -> Vec<InlineSpan> {
    let mut merged: Vec<InlineSpan> = Vec::new();
    for span in spans {
        match (merged.last_mut(), span) {
            (
                Some(InlineSpan::Text {
                    content,
                    styles: existing_styles,
                }),
                InlineSpan::Text {
                    content: next_content,
                    styles: next_styles,
                },
            ) if *existing_styles == next_styles => content.push_str(&next_content),
            (_, other) => merged.push(other),
        }
    }
    merged
}

pub fn decode_basic_entities(value: &str) -> String {
    value
        .replace("&quot;", "\"")
        .replace("&gt;", ">")
        .replace("&lt;", "<")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::{parse_inline_spans, InlineSpan};

    #[test]
    fn parses_equations_styles_and_hyperlinks() {
        let spans = parse_inline_spans(
            r#"A <text style="bold,italic">B&amp;C</text> <eq>x+1</eq> <hyperlink><text style="underline">site</text><url>https://example.com?a=1&amp;b=2</url></hyperlink>"#,
        );

        assert_eq!(
            spans,
            vec![
                InlineSpan::Text {
                    content: "A ".to_string(),
                    styles: Vec::new()
                },
                InlineSpan::Text {
                    content: "B&C".to_string(),
                    styles: vec!["bold".to_string(), "italic".to_string()]
                },
                InlineSpan::Text {
                    content: " ".to_string(),
                    styles: Vec::new()
                },
                InlineSpan::InlineEquation {
                    content: "x+1".to_string()
                },
                InlineSpan::Text {
                    content: " ".to_string(),
                    styles: Vec::new()
                },
                InlineSpan::Hyperlink {
                    content: "site".to_string(),
                    url: "https://example.com?a=1&b=2".to_string(),
                    styles: vec!["underline".to_string()]
                },
            ]
        );
    }

    #[test]
    fn merges_adjacent_text_spans_with_same_style() {
        let spans =
            parse_inline_spans(r#"<text style="bold">A</text><text style="bold">B</text>C"#);

        assert_eq!(
            spans,
            vec![
                InlineSpan::Text {
                    content: "AB".to_string(),
                    styles: vec!["bold".to_string()]
                },
                InlineSpan::Text {
                    content: "C".to_string(),
                    styles: Vec::new()
                },
            ]
        );
    }
}
