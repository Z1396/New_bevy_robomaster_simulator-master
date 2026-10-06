//! Main world -> render world hand-off.

use bevy::camera::{Camera, MainPassResolutionOverride, Projection};
use bevy::prelude::*;
use bevy::render::{Extract, MainWorld, sync_world::RenderEntity};

use crate::{MetalFxResetHistory, MetalFxTemporalUpscaling};

use super::context::MetalFxContext;

/// Wall-clock delta of the frame being rendered; `MTLFXFrameInterpolator` needs it to place the
/// interpolated frame in time.
#[derive(Resource, Default)]
pub(super) struct MetalFxFrameDelta(pub f32);

pub(super) fn extract_frame_delta(mut delta: ResMut<MetalFxFrameDelta>, time: Extract<Res<Time>>) {
    delta.0 = time.delta_secs().max(f32::EPSILON);
}

pub(super) fn extract_metalfx(
    mut commands: Commands,
    mut main_world: ResMut<MainWorld>,
    extracted: Query<Has<MetalFxTemporalUpscaling>>,
) {
    let mut cameras = main_world.query::<(
        Entity,
        RenderEntity,
        &Camera,
        &Projection,
        Option<&MetalFxTemporalUpscaling>,
        Has<MetalFxResetHistory>,
    )>();

    // Reset requests are one-shot: acknowledge them here so a request made at any point during the
    // main-world frame reaches the render world exactly once.
    let mut acknowledged_resets = Vec::new();

    for (main_entity, render_entity, camera, projection, metalfx, reset) in
        cameras.iter(&main_world)
    {
        let Ok(mut entity_commands) = commands.get_entity(render_entity) else {
            continue;
        };

        let enabled = metalfx
            .filter(|_| camera.is_active && projection.is_perspective())
            .copied();

        match enabled {
            Some(metalfx) => {
                entity_commands.insert(metalfx);
                if reset {
                    entity_commands.insert(MetalFxResetHistory);
                    acknowledged_resets.push(main_entity);
                } else {
                    entity_commands.remove::<MetalFxResetHistory>();
                }
            }
            None if extracted.get(render_entity) == Ok(true) => {
                entity_commands.remove::<(
                    MetalFxTemporalUpscaling,
                    MetalFxResetHistory,
                    MetalFxContext,
                    MainPassResolutionOverride,
                )>();
            }
            None => {}
        }
    }

    for entity in acknowledged_resets {
        if let Ok(mut entity) = main_world.get_entity_mut(entity) {
            entity.remove::<MetalFxResetHistory>();
        }
    }
}
