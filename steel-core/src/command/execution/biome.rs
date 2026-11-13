//! Biome registry command arguments.

use steel_registry::{
    BIOMES_REGISTRY, REGISTRY, RegistryExt as _, TaggedRegistryExt as _, biome::BiomeRef,
};
use steel_utils::{Identifier, translations};

use super::argument::{parse_identifier, unknown_resource};
use super::suggestions::identifier_matches;
use crate::command::brigadier::{
    CommandSyntaxError, CommandSyntaxErrorKind, StringReader, SuggestionsBuilder,
};

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
    if reader.peek() != Some('#') {
        let key = parse_identifier(reader)?;
        return REGISTRY.biomes.by_key(&key).map_or_else(
            || Err(unknown_resource(reader, &key, &BIOMES_REGISTRY)),
            |biome| Ok(BiomeOrTag::Biome(biome)),
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
    if REGISTRY.biomes.tag_keys().any(|tag| tag == &key) {
        return Ok(BiomeOrTag::Tag(key));
    }

    let message = translations::ARGUMENT_RESOURCE_TAG_NOT_FOUND
        .message([key.to_string(), BIOMES_REGISTRY.to_string()])
        .component();
    let error = reader.error(CommandSyntaxErrorKind::Dynamic(Box::new(message)));
    reader.restore(start);
    Err(error)
}

/// Suggests biomes and biome tags. Like vanilla, tags are only offered for empty input or
/// input starting with `#`.
pub(super) fn suggest_biomes(builder: &mut SuggestionsBuilder<'_>) {
    let remaining = builder.remaining_lowercase();
    let tags = |prefix: &str| {
        REGISTRY
            .biomes
            .tag_keys()
            .filter(|tag| identifier_matches(prefix, tag))
            .map(|tag| format!("#{tag}"))
            .collect::<Vec<_>>()
    };
    let suggestions = if let Some(tag_prefix) = remaining.strip_prefix('#') {
        tags(tag_prefix)
    } else {
        let mut biomes = REGISTRY
            .biomes
            .iter()
            .filter(|(_, biome)| identifier_matches(remaining, &biome.key))
            .map(|(_, biome)| biome.key.to_string())
            .collect::<Vec<_>>();
        if remaining.is_empty() {
            biomes.extend(tags(remaining));
        }
        biomes
    };
    for suggestion in suggestions {
        builder.suggest(suggestion);
    }
}
