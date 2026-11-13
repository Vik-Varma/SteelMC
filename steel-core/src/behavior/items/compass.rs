use crate::player::player_inventory::Prediction;
use std::borrow::Cow;

use steel_macros::item_behavior;
use steel_protocol::packets::game::SoundSource;
use steel_registry::{
    RegistryEntry as _,
    blocks::block_state_ext::BlockStateExt,
    data_components::{
        components::{GlobalPos, LodestoneTracker},
        vanilla_components::LODESTONE_TRACKER,
    },
    item_stack::ItemStack,
    sound_events, vanilla_blocks, vanilla_poi_types,
};
use text_components::TextComponent;

use crate::behavior::{InteractionResult, InventoryTickContext, ItemBehavior, UseOnContext};
use crate::inventory::container::Container;
use crate::poi::UnloadedPoi;

use super::dynamic_name::{default_name, translated};

/// Compass item behavior.
#[item_behavior]
pub struct CompassItem;

impl ItemBehavior for CompassItem {
    fn get_name<'a>(&self, stack: &'a ItemStack) -> Cow<'a, TextComponent> {
        if stack.has(LODESTONE_TRACKER) {
            translated("item.minecraft.lodestone_compass".to_owned(), None)
        } else {
            default_name(stack)
        }
    }

    fn use_on(&self, context: &mut UseOnContext) -> InteractionResult {
        let pos = context.hit_result.block_pos;
        let block_state = context.world.get_block_state(pos);

        if block_state.get_block() != &vanilla_blocks::LODESTONE {
            return InteractionResult::Pass;
        }

        context.world.play_sound(
            &sound_events::ITEM_LODESTONE_COMPASS_LOCK,
            SoundSource::Players,
            pos,
            1.0,
            1.0,
            None,
        );

        let dimension = context.world.key.clone();
        let tracker = LodestoneTracker::new(Some(GlobalPos::new(dimension, pos)), true);

        let has_infinite_materials = context.player.has_infinite_materials();

        let leftover = context.inv.with_inventory(|inv| {
            if !has_infinite_materials && inv.get_item_in_hand(context.hand).count() == 1 {
                inv.mutate_item_in_hand(context.hand, |item| item.set(LODESTONE_TRACKER, tracker));
                return ItemStack::empty();
            }

            let mut lodestone_compass = inv.mutate_item_in_hand(context.hand, |item| {
                item.consume_and_return(1, has_infinite_materials)
            });
            lodestone_compass.set(LODESTONE_TRACKER, tracker);
            if inv.add(&mut lodestone_compass) || has_infinite_materials {
                ItemStack::empty()
            } else {
                lodestone_compass
            }
        });

        if !leftover.is_empty() {
            let _ = context
                .player
                .drop_item(leftover, false, Prediction::Predicted);
        }

        InteractionResult::Success
    }

    fn inventory_tick(&self, context: &mut InventoryTickContext<'_>) {
        let Some(target) = context
            .with_item(|stack| {
                stack
                    .get(LODESTONE_TRACKER)
                    .filter(|tracker| tracker.tracked())
                    .and_then(|tracker| tracker.target().cloned())
            })
            .flatten()
        else {
            return;
        };

        let world = context.world;
        if world.key != *target.dimension() {
            return;
        }

        let pos = target.pos();
        if world.is_in_world_bounds(pos) {
            let poi_type = if world.is_full_chunk_loaded_at(pos) {
                world.poi_storage.lock().get_type(pos)
            } else {
                // Vanilla reads the target's POI data from disk. Steel reads the chunk in the
                // background and keeps the target until the answer arrives.
                let UnloadedPoi::Known(poi_type) = world.unloaded_poi_type_at(pos) else {
                    return;
                };
                poi_type
            };
            let lodestone_exists = vanilla_poi_types::LODESTONE
                .try_id()
                .is_some_and(|id| poi_type == Some(id));
            if lodestone_exists {
                return;
            }
        }

        context.with_item(|stack| stack.set(LODESTONE_TRACKER, LodestoneTracker::new(None, true)));
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::Arc,
        thread,
        time::{Duration, Instant},
    };

    use super::*;
    use crate::behavior::init_behaviors;
    use crate::entity::{Entity, LivingEntity};
    use crate::player::Player;
    use crate::test_support::{
        TestPlayerBuilder, block_pool, fresh_test_world, insert_ready_full_chunk,
    };
    use glam::DVec3;
    use steel_registry::blocks::properties::Direction;
    use steel_registry::items::item::BlockHitResult;
    use steel_registry::{init_vanilla_registry, vanilla_items};
    use steel_utils::types::InteractionHand;
    use steel_utils::types::UpdateFlags;
    use steel_utils::{BlockPos, ChunkPos};
    use uuid::Uuid;

    #[test]
    fn compass_binds_to_lodestone_and_invalidates_when_removed() {
        init_vanilla_registry();
        init_behaviors();

        let world_fixture = fresh_test_world("compass_test_world");
        let world = &world_fixture.world;
        let pos = BlockPos::new(0, 64, 0);
        let chunk_pos = ChunkPos::from_block_pos(pos);
        insert_ready_full_chunk(world, chunk_pos);

        assert!(world.set_block(
            pos,
            vanilla_blocks::LODESTONE.default_state(),
            UpdateFlags::UPDATE_ALL
        ));

        let player = TestPlayerBuilder::new(Arc::clone(world), "CompassTester", 1)
            .uuid(Uuid::from_u128(1))
            .build();

        player
            .inventory
            .lock()
            .set_selected_item(ItemStack::new(&vanilla_items::COMPASS));

        let hit_result = BlockHitResult {
            location: DVec3::new(0.5, 64.5, 0.5),
            direction: Direction::Up,
            block_pos: pos,
            miss: false,
            inside: false,
            world_border_hit: false,
        };
        let mut context = UseOnContext::new(
            &player,
            InteractionHand::MainHand,
            hit_result,
            world,
            Arc::clone(&player.inventory),
        );

        let behavior = CompassItem;

        let result = behavior.use_on(&mut context);
        assert_eq!(result, InteractionResult::Success);

        let compass = player.inventory.lock().get_selected_item().clone();
        let tracker = compass
            .get(LODESTONE_TRACKER)
            .expect("should have lodestone tracker component");
        assert!(tracker.tracked());
        let target = tracker.target().expect("should have target global pos");
        assert_eq!(target.pos(), pos);
        assert_eq!(*target.dimension(), world.key);

        Arc::clone(&player).tick();
        let compass = player.inventory.lock().get_selected_item().clone();
        let tracker = compass
            .get(LODESTONE_TRACKER)
            .expect("should keep lodestone tracker component");
        assert!(tracker.target().is_some());

        assert!(world.set_block(
            pos,
            vanilla_blocks::AIR.default_state(),
            UpdateFlags::UPDATE_ALL
        ));
        Arc::clone(&player).tick();
        let compass = player.inventory.lock().get_selected_item().clone();
        let tracker = compass
            .get(LODESTONE_TRACKER)
            .expect("should keep lodestone tracker component");
        assert!(
            tracker.target().is_none(),
            "target should be invalidated and set to None"
        );
    }

    #[test]
    fn compass_invalidates_target_outside_world_bounds() {
        init_vanilla_registry();
        init_behaviors();

        let world_fixture = fresh_test_world("compass_bounds_test_world");
        let world = &world_fixture.world;
        let player = TestPlayerBuilder::new(Arc::clone(world), "CompassTester", 1)
            .uuid(Uuid::from_u128(1))
            .build();

        let invalid_pos = BlockPos::new(40_000_000, 64, 0);
        let tracker =
            LodestoneTracker::new(Some(GlobalPos::new(world.key.clone(), invalid_pos)), true);

        let mut compass = ItemStack::new(&vanilla_items::COMPASS);
        compass.set(LODESTONE_TRACKER, tracker);

        player.inventory.lock().set_offhand_item(compass);
        LivingEntity::tick_equipment(player.as_ref());

        let compass = player.inventory.lock().get_offhand_item().clone();
        let tracker = compass
            .get(LODESTONE_TRACKER)
            .expect("should keep lodestone tracker component");
        assert!(
            tracker.target().is_none(),
            "target should be immediately invalidated and set to None"
        );
    }

    /// Ticks `player` until the selected compass's target is `expected`, failing after a few
    /// seconds. The compass checks unloaded targets in the background.
    fn tick_until_target(player: &Arc<Player>, expected: Option<BlockPos>) {
        let started = Instant::now();
        loop {
            Arc::clone(player).tick();
            let target = player
                .inventory
                .lock()
                .get_selected_item()
                .get(LODESTONE_TRACKER)
                .and_then(|tracker| tracker.target().map(GlobalPos::pos));
            if target == expected {
                return;
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "the target stayed {target:?}, expected {expected:?}"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn compass_checks_targets_in_unloaded_chunks_against_their_save() {
        use crate::block_entity::init_block_entities;
        use crate::chunk::status::ChunkStatus;
        use crate::chunk_saver::ChunkStorage;

        init_vanilla_registry();
        init_behaviors();
        init_block_entities();
        let world_fixture = fresh_test_world("compass_unloaded_target");
        let world = &world_fixture.world;

        // Save a chunk holding a lodestone, then unload it.
        let saved_chunk = ChunkPos::new(2, 0);
        let lodestone = BlockPos::new(40, 70, 2);
        let holder = insert_ready_full_chunk(world, saved_chunk);
        assert!(world.set_block(
            lodestone,
            vanilla_blocks::LODESTONE.default_state(),
            UpdateFlags::UPDATE_ALL
        ));
        let Some(chunk) = holder.try_chunk(ChunkStatus::Full) else {
            panic!("inserted chunk should be Full");
        };
        let Some(prepared) = ChunkStorage::prepare_chunk_save(chunk, ChunkStatus::Full, &[], true)
        else {
            panic!("a forced save should be prepared");
        };
        let saved = world.chunk_map.chunk_runtime.block_on(
            world
                .chunk_map
                .storage
                .save_chunk_data(prepared, &world.chunk_map.generation_pool),
        );
        assert!(matches!(saved, Ok(true)));
        let _ = world.chunk_map.chunks.remove_sync(&saved_chunk);
        world.poi_storage.lock().remove_chunk(saved_chunk);
        drop(holder);
        assert!(!world.is_full_chunk_loaded_at(lodestone));

        let player = TestPlayerBuilder::new(Arc::clone(world), "CompassTester", 1)
            .uuid(Uuid::from_u128(1))
            .build();
        let compass_for = |target: BlockPos| {
            let mut compass = ItemStack::new(&vanilla_items::COMPASS);
            compass.set(
                LODESTONE_TRACKER,
                LodestoneTracker::new(Some(GlobalPos::new(world.key.clone(), target)), true),
            );
            compass
        };

        // The saved lodestone keeps its compass, both while the chunk is read and after.
        player
            .inventory
            .lock()
            .set_selected_item(compass_for(lodestone));
        {
            // Hold the read back so these ticks find it still running.
            let _blocked = block_pool(&world.chunk_map.generation_pool);
            for _ in 0..5 {
                tick_until_target(&player, Some(lodestone));
            }
        }
        for _ in 0..20 {
            tick_until_target(&player, Some(lodestone));
            thread::sleep(Duration::from_millis(5));
        }
        // The read found the saved lodestone.
        let started = Instant::now();
        let lodestone_type = vanilla_poi_types::LODESTONE.try_id();
        loop {
            match world.unloaded_poi_type_at(lodestone) {
                UnloadedPoi::Known(found) => {
                    assert_eq!(found, lodestone_type);
                    break;
                }
                UnloadedPoi::Unknown => {
                    assert!(started.elapsed() < Duration::from_secs(5));
                    thread::sleep(Duration::from_millis(5));
                }
            }
        }
        tick_until_target(&player, Some(lodestone));

        // No lodestone is saved at this position of the same chunk.
        let beside = BlockPos::new(41, 70, 2);
        player
            .inventory
            .lock()
            .set_selected_item(compass_for(beside));
        Arc::clone(&player).tick();
        let kept = player
            .inventory
            .lock()
            .get_selected_item()
            .get(LODESTONE_TRACKER)
            .and_then(|tracker| tracker.target().map(GlobalPos::pos));
        assert_eq!(
            kept,
            Some(beside),
            "the target is kept until the read finishes"
        );
        tick_until_target(&player, None);

        // A chunk that was never saved holds no lodestone.
        player
            .inventory
            .lock()
            .set_selected_item(compass_for(BlockPos::new(85, 70, 2)));
        tick_until_target(&player, None);
    }
}
