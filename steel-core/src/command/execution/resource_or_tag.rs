//! Parsing and suggestions shared by vanilla `ResourceOrTagArgument` command arguments.

use steel_registry::{RegistryEntry, TaggedRegistryExt};
use steel_utils::{Identifier, translations};

use super::argument::{parse_identifier, unknown_resource};
use super::suggestions::identifier_matches;
use crate::command::brigadier::{
    CommandSyntaxError, CommandSyntaxErrorKind, StringReader, SuggestionsBuilder,
};

/// A registry entry or a tag of that registry, as typed in a command.
pub(super) enum ResourceOrTag<E: 'static> {
    Resource(&'static E),
    Tag(Identifier),
}

/// Parses an entry key of `registry`, or `#` and one of its tags.
pub(super) fn parse_resource_or_tag<R: TaggedRegistryExt>(
    reader: &mut StringReader<'_>,
    registry: &R,
    registry_id: &Identifier,
) -> Result<ResourceOrTag<R::Entry>, CommandSyntaxError> {
    if reader.peek() != Some('#') {
        let key = parse_identifier(reader)?;
        return registry.by_key(&key).map_or_else(
            || Err(unknown_resource(reader, &key, registry_id)),
            |entry| Ok(ResourceOrTag::Resource(entry)),
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
    if registry.tag_keys().any(|tag| tag == &key) {
        return Ok(ResourceOrTag::Tag(key));
    }

    let message = translations::ARGUMENT_RESOURCE_TAG_NOT_FOUND
        .message([key.to_string(), registry_id.to_string()])
        .component();
    let error = reader.error(CommandSyntaxErrorKind::Dynamic(Box::new(message)));
    reader.restore(start);
    Err(error)
}

/// Suggests `registry`'s entries and tags. Like vanilla, tags are only offered for empty input
/// or input starting with `#`.
pub(super) fn suggest_resource_or_tag<R: TaggedRegistryExt>(
    builder: &mut SuggestionsBuilder<'_>,
    registry: &R,
) {
    let remaining = builder.remaining_lowercase();
    let tags = |prefix: &str| {
        registry
            .tag_keys()
            .filter(|tag| identifier_matches(prefix, tag))
            .map(|tag| format!("#{tag}"))
            .collect::<Vec<_>>()
    };
    let suggestions = if let Some(tag_prefix) = remaining.strip_prefix('#') {
        tags(tag_prefix)
    } else {
        let mut entries = (0..registry.len())
            .filter_map(|id| registry.by_id(id))
            .filter(|entry| identifier_matches(remaining, entry.key()))
            .map(|entry| entry.key().to_string())
            .collect::<Vec<_>>();
        if remaining.is_empty() {
            entries.extend(tags(remaining));
        }
        entries
    };
    for suggestion in suggestions {
        builder.suggest(suggestion);
    }
}
