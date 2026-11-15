//! Location commands.

mod biome;
mod poi;
mod structure;

use std::time::Duration;

use steel_utils::{BlockPos, Identifier, translations};
use text_components::{
    Modifier, TextComponent,
    format::Color,
    interactivity::{ClickEvent, HoverEvent},
    translation::Translation,
};

use super::super::{
    brigadier::CommandNodeBuilder,
    execution::{CommandSource, SteelArgumentType, SteelCommandRuntime, argument, literal},
    registration::CommandRegistration,
};

pub(super) fn registration() -> CommandRegistration<CommandSource> {
    CommandRegistration::new(Identifier::vanilla_static("locate"), |_| command())
}

fn command() -> CommandNodeBuilder<CommandSource, SteelCommandRuntime> {
    literal("locate")
        .then(
            literal("structure").then(
                argument("structure", SteelArgumentType::structure_or_tag_key())
                    .executes_suspended(structure::start_search),
            ),
        )
        .then(
            literal("biome").then(
                argument("biome", SteelArgumentType::biome_or_tag())
                    .executes_suspended(biome::start_search),
            ),
        )
        .then(
            literal("poi").then(
                argument("poi", SteelArgumentType::poi_type_or_tag())
                    .executes_suspended(poi::start_search),
            ),
        )
}

/// Sends the locate success message and returns the distance vanilla reports.
///
/// With `include_y` the distance is 3D and the target's Y is shown; otherwise the
/// distance is horizontal and Y is shown as `~`.
fn show_locate_result(
    source: &CommandSource,
    origin: BlockPos,
    found: BlockPos,
    success: &'static Translation<3>,
    include_y: bool,
    found_name: &str,
    elapsed: Duration,
) -> i32 {
    let distance = if include_y {
        distance_3d(origin, found)
    } else {
        horizontal_distance(origin, found)
    };
    let component = success
        .message([
            TextComponent::from(found_name.to_owned()),
            locate_coordinates_component(found, include_y),
            TextComponent::from(distance.to_string()),
        ])
        .component();
    source.send_success(&component, false);
    tracing::info!(
        "Locating element {} took {} ms",
        found_name,
        elapsed.as_millis()
    );
    distance
}

fn horizontal_distance(a: BlockPos, b: BlockPos) -> i32 {
    let dx = b.0.x.wrapping_sub(a.0.x);
    let dz = b.0.z.wrapping_sub(a.0.z);
    let squared = dx.wrapping_mul(dx).wrapping_add(dz.wrapping_mul(dz));
    (f64::from(squared as f32).sqrt() as f32).floor() as i32
}

/// `Mth.floor(Mth.sqrt((float) a.distSqr(b)))`, where `distSqr` works in doubles.
fn distance_3d(a: BlockPos, b: BlockPos) -> i32 {
    let squared = a.dist_sqr(b) as f32;
    (f64::from(squared).sqrt() as f32).floor() as i32
}

fn locate_coordinates_component(pos: BlockPos, include_y: bool) -> TextComponent {
    let displayed_y = if include_y {
        pos.0.y.to_string()
    } else {
        "~".to_owned()
    };
    let coordinates = translations::CHAT_COORDINATES
        .message([
            TextComponent::from(pos.0.x.to_string()),
            TextComponent::from(displayed_y.clone()),
            TextComponent::from(pos.0.z.to_string()),
        ])
        .component();
    translations::CHAT_SQUARE_BRACKETS
        .message([coordinates])
        .component()
        .color(Color::Green)
        .hover_event(HoverEvent::show_text(
            &translations::CHAT_COORDINATES_TOOLTIP,
        ))
        .click_event(ClickEvent::suggest_command(format!(
            "/tp @s {} {} {}",
            pos.0.x, displayed_y, pos.0.z
        )))
}

#[cfg(test)]
mod tests {
    use super::super::create_dispatcher;
    use super::*;
    use crate::command::brigadier::{CommandDispatcher, NodeId};
    use steel_registry::init_vanilla_registry;
    use text_components::content::Content;

    type Dispatcher = CommandDispatcher<CommandSource, SteelCommandRuntime>;

    fn child(dispatcher: &Dispatcher, parent: NodeId, name: &str) -> NodeId {
        let Some(children) = dispatcher.children(parent) else {
            panic!("parent node should exist");
        };
        let Some(child) = children.iter().copied().find(|child| {
            dispatcher
                .node(*child)
                .is_some_and(|node| node.name() == name)
        }) else {
            panic!("child {name} should exist");
        };
        child
    }

    #[test]
    fn locate_graph_exposes_the_supported_typed_branches() {
        init_vanilla_registry();
        let Ok(dispatcher) = create_dispatcher() else {
            panic!("built-in commands should register");
        };
        let locate = child(&dispatcher, dispatcher.root(), "locate");
        for (branch, argument_type) in [
            ("structure", SteelArgumentType::structure_or_tag_key()),
            ("biome", SteelArgumentType::biome_or_tag()),
            ("poi", SteelArgumentType::poi_type_or_tag()),
        ] {
            let literal = child(&dispatcher, locate, branch);
            let target = child(&dispatcher, literal, branch);
            let Some(target_node) = dispatcher.node(target) else {
                panic!("locate {branch} argument should exist");
            };
            assert_eq!(target_node.argument_type(), Some(&argument_type));
            assert!(target_node.is_executable());
            assert!(dispatcher.children(target).is_some_and(<[_]>::is_empty));
        }
        assert_eq!(dispatcher.children(locate).map(<[_]>::len), Some(3));
    }

    #[test]
    fn locate_coordinates_component_matches_vanilla_interactivity() {
        let component = locate_coordinates_component(BlockPos::new(12, 0, -34), false);

        // `ComponentUtils.wrapInSquareBrackets` around `chat.coordinates`, styled on the outside.
        let Content::Translate(brackets) = &component.content else {
            panic!("coordinates should be wrapped in a translated component");
        };
        assert_eq!(brackets.key, "chat.square_brackets");
        let Some([coordinates]) = brackets.args.as_deref() else {
            panic!("square brackets should wrap exactly the coordinates");
        };
        assert!(matches!(
            &coordinates.content,
            Content::Translate(message) if message.key == "chat.coordinates"
        ));
        assert_eq!(component.children, [] as [TextComponent; 0]);
        assert_eq!(component.format.color, Some(Color::Green));
        assert!(matches!(
            component.interactions.click,
            Some(ClickEvent::SuggestCommand { ref command })
                if command.as_ref() == "/tp @s 12 ~ -34"
        ));
        assert!(matches!(
            component.interactions.hover,
            Some(HoverEvent::ShowText { .. })
        ));

        let with_y = locate_coordinates_component(BlockPos::new(12, -40, -34), true);
        assert!(matches!(
            with_y.interactions.click,
            Some(ClickEvent::SuggestCommand { ref command })
                if command.as_ref() == "/tp @s 12 -40 -34"
        ));
    }

    #[test]
    fn horizontal_distance_matches_vanillas_wrapping_int_and_float_math() {
        assert_eq!(
            horizontal_distance(BlockPos::new(0, 0, 0), BlockPos::new(3, 100, 4)),
            5
        );
        assert_eq!(
            horizontal_distance(
                BlockPos::new(-30_000_000, 0, 0),
                BlockPos::new(30_000_000, 0, 0)
            ),
            36_907
        );
    }

    #[test]
    fn distance_3d_uses_doubles_then_float_precision() {
        assert_eq!(
            distance_3d(BlockPos::new(0, 0, 0), BlockPos::new(2, 3, 6)),
            7
        );
        // No int wrapping: the full 60M span survives, then rounds through f32.
        assert_eq!(
            distance_3d(
                BlockPos::new(-30_000_000, 0, 0),
                BlockPos::new(30_000_000, 0, 0)
            ),
            60_000_000
        );
    }
}
