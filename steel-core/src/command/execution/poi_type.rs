//! Point-of-interest type registry command arguments.

use steel_registry::{
    POI_TYPE_REGISTRY, REGISTRY, RegistryExt as _, TaggedRegistryExt as _, poi::PoiTypeRef,
};
use steel_utils::{Identifier, translations};

use super::argument::{parse_identifier, unknown_resource};
use super::suggestions::identifier_matches;
use crate::command::brigadier::{
    CommandSyntaxError, CommandSyntaxErrorKind, StringReader, SuggestionsBuilder,
};

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
    if reader.peek() != Some('#') {
        let key = parse_identifier(reader)?;
        return REGISTRY.poi_types.by_key(&key).map_or_else(
            || Err(unknown_resource(reader, &key, &POI_TYPE_REGISTRY)),
            |poi_type| Ok(PoiTypeOrTag::PoiType(poi_type)),
        );
    }

    let start = reader.checkpoint();
    reader.skip();
    let key = match parse_identifier(reader) {
        Ok(key) => key,
        Err(error) => {
            reader.restore(start);
            return Err(error);
        }
    };
    if REGISTRY.poi_types.tag_keys().any(|tag| tag == &key) {
        return Ok(PoiTypeOrTag::Tag(key));
    }

    let message = translations::ARGUMENT_RESOURCE_TAG_NOT_FOUND
        .message([key.to_string(), POI_TYPE_REGISTRY.to_string()])
        .component();
    let error = reader.error(CommandSyntaxErrorKind::Dynamic(Box::new(message)));
    reader.restore(start);
    Err(error)
}

/// Suggests POI types and POI type tags. Like vanilla, tags are only offered for empty input or
/// input starting with `#`.
pub(super) fn suggest_poi_types(builder: &mut SuggestionsBuilder<'_>) {
    let remaining = builder.remaining_lowercase();
    let tags = |prefix: &str| {
        REGISTRY
            .poi_types
            .tag_keys()
            .filter(|tag| identifier_matches(prefix, tag))
            .map(|tag| format!("#{tag}"))
            .collect::<Vec<_>>()
    };
    let suggestions = if let Some(tag_prefix) = remaining.strip_prefix('#') {
        tags(tag_prefix)
    } else {
        let mut poi_types = REGISTRY
            .poi_types
            .iter()
            .filter(|(_, poi_type)| identifier_matches(remaining, &poi_type.key))
            .map(|(_, poi_type)| poi_type.key.to_string())
            .collect::<Vec<_>>();
        if remaining.is_empty() {
            poi_types.extend(tags(remaining));
        }
        poi_types
    };
    for suggestion in suggestions {
        builder.suggest(suggestion);
    }
}
