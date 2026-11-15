//! Biome registry command arguments.

use steel_registry::{BIOMES_REGISTRY, REGISTRY, biome::BiomeRef};
use steel_utils::Identifier;

use super::resource_or_tag::{ResourceOrTag, parse_resource_or_tag};
use crate::command::brigadier::{CommandSyntaxError, StringReader};

/// A registered biome or biome tag retained by a command argument.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum BiomeOrTag {
    Biome(BiomeRef),
    Tag(Identifier),
}

impl BiomeOrTag {
    pub(crate) fn matches(&self, biome: BiomeRef) -> bool {
        match self {
            Self::Biome(expected) => *expected == biome,
            Self::Tag(tag) => biome.has_tag(tag),
        }
    }

    pub(crate) fn as_printable(&self) -> String {
        match self {
            Self::Biome(biome) => biome.key.to_string(),
            Self::Tag(tag) => format!("#{tag}"),
        }
    }

    /// Names a search result, adding the matched biome when the query is a tag.
    pub(crate) fn found_name(&self, found: BiomeRef) -> String {
        match self {
            Self::Biome(_) => self.as_printable(),
            Self::Tag(tag) => format!("#{tag} ({})", found.key),
        }
    }
}

pub(super) fn parse_biome_or_tag(
    reader: &mut StringReader<'_>,
) -> Result<BiomeOrTag, CommandSyntaxError> {
    Ok(
        match parse_resource_or_tag(reader, &REGISTRY.biomes, &BIOMES_REGISTRY)? {
            ResourceOrTag::Resource(entry) => BiomeOrTag::Biome(entry),
            ResourceOrTag::Tag(tag) => BiomeOrTag::Tag(tag),
        },
    )
}
