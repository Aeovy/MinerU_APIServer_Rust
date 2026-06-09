use quick_xml::events::Event;

use crate::{
    error::{ApiError, ApiResult},
    office::package::{local_name, rels_path_for_part, resolve_part_path, OoxmlPackage},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relationship {
    pub id: String,
    pub rel_type: String,
    pub target: String,
    pub target_mode: Option<String>,
}

impl Relationship {
    pub fn is_external(&self) -> bool {
        self.target_mode
            .as_deref()
            .is_some_and(|mode| mode.eq_ignore_ascii_case("External"))
    }

    pub fn target_part(&self, base_part: &str) -> Option<String> {
        (!self.is_external()).then(|| resolve_part_path(base_part, &self.target))
    }
}

pub fn read_relationships(package: &OoxmlPackage, part_name: &str) -> ApiResult<Vec<Relationship>> {
    let rels_path = rels_path_for_part(part_name);
    let Some(bytes) = package.read(&rels_path) else {
        return Ok(Vec::new());
    };
    parse_relationships(bytes)
}

pub fn relationship_target_part(
    package: &OoxmlPackage,
    base_part: &str,
    rel_id: &str,
) -> ApiResult<Option<String>> {
    Ok(read_relationships(package, base_part)?
        .into_iter()
        .find(|rel| rel.id == rel_id)
        .and_then(|rel| rel.target_part(base_part)))
}

pub fn parse_relationships(bytes: &[u8]) -> ApiResult<Vec<Relationship>> {
    let mut reader = quick_xml::Reader::from_reader(std::io::Cursor::new(bytes));
    reader.config_mut().trim_text(true);
    let mut buffer = Vec::new();
    let mut relationships = Vec::new();
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(event)) | Ok(Event::Empty(event)) => {
                if local_name(event.name().as_ref()) == b"Relationship" {
                    let mut id = None;
                    let mut rel_type = None;
                    let mut target = None;
                    let mut target_mode = None;
                    for attr in event.attributes().flatten() {
                        let key = local_name(attr.key.as_ref());
                        let value = attr
                            .decoded_and_normalized_value(
                                quick_xml::XmlVersion::Implicit1_0,
                                reader.decoder(),
                            )
                            .map_err(|error| ApiError::BadRequest(error.to_string()))?
                            .into_owned();
                        match key {
                            b"Id" => id = Some(value),
                            b"Type" => rel_type = Some(value),
                            b"Target" => target = Some(value),
                            b"TargetMode" => target_mode = Some(value),
                            _ => {}
                        }
                    }
                    if let (Some(id), Some(rel_type), Some(target)) = (id, rel_type, target) {
                        relationships.push(Relationship {
                            id,
                            rel_type,
                            target,
                            target_mode,
                        });
                    }
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(error) => return Err(ApiError::BadRequest(error.to_string())),
        }
        buffer.clear();
    }
    Ok(relationships)
}

#[cfg(test)]
mod tests {
    use super::parse_relationships;

    #[test]
    fn parses_relationships() {
        let rels = parse_relationships(
            br#"<Relationships><Relationship Id="rId1" Type="image" Target="media/a.png"/><Relationship Id="rId2" Type="hyperlink" Target="https://example.com" TargetMode="External"/></Relationships>"#,
        )
        .unwrap();
        assert_eq!(rels.len(), 2);
        assert_eq!(rels[0].id, "rId1");
        assert!(rels[1].is_external());
    }
}
