//! Point-of-interest type registry command arguments.

use steel_registry::{POI_TYPE_REGISTRY, REGISTRY, TaggedRegistryExt as _, poi::PoiTypeRef};
use steel_utils::Identifier;

use super::resource_or_tag::{ResourceOrTag, parse_resource_or_tag};
use crate::command::brigadier::{CommandSyntaxError, StringReader};

/// A registered POI type or POI type tag retained by a command argument.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PoiTypeOrTag {
    PoiType(PoiTypeRef),
    Tag(Identifier),
}

impl PoiTypeOrTag {
    pub(crate) fn matches(&self, poi_type: PoiTypeRef) -> bool {
        match self {
            Self::PoiType(expected) => *expected == poi_type,
            Self::Tag(tag) => REGISTRY.poi_types.is_in_tag(poi_type, tag),
        }
    }

    pub(crate) fn as_printable(&self) -> String {
        match self {
            Self::PoiType(poi_type) => poi_type.key.to_string(),
            Self::Tag(tag) => format!("#{tag}"),
        }
    }

    /// Names a search result, adding the matched type when the query is a tag.
    pub(crate) fn found_name(&self, found: PoiTypeRef) -> String {
        match self {
            Self::PoiType(_) => self.as_printable(),
            Self::Tag(tag) => format!("#{tag} ({})", found.key),
        }
    }
}

pub(super) fn parse_poi_type_or_tag(
    reader: &mut StringReader<'_>,
) -> Result<PoiTypeOrTag, CommandSyntaxError> {
    Ok(
        match parse_resource_or_tag(reader, &REGISTRY.poi_types, &POI_TYPE_REGISTRY)? {
            ResourceOrTag::Resource(entry) => PoiTypeOrTag::PoiType(entry),
            ResourceOrTag::Tag(tag) => PoiTypeOrTag::Tag(tag),
        },
    )
}
