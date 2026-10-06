//! macOS render-world implementation, wired into Bevy's 3D pipeline.

mod context;
mod extract;
mod metal;
mod node;
mod prepare;

use bevy::core_pipeline::{Core3d, Core3dSystems};
use bevy::prelude::*;
use bevy::render::{ExtractSchedule, Render, RenderApp, RenderSystems, view::prepare_view_targets};

pub(crate) fn build(app: &mut App) {
    let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
        return;
    };

    render_app
        .init_resource::<extract::MetalFxFrameDelta>()
        .add_systems(
            ExtractSchedule,
            (extract::extract_frame_delta, extract::extract_metalfx),
        )
        .add_systems(
            Render,
            prepare::prepare_metalfx
                .in_set(RenderSystems::PrepareViews)
                // The main texture is allocated here, and we still need to add STORAGE_BINDING to it.
                .before(prepare_view_targets),
        )
        .add_systems(
            Core3d,
            node::metalfx_upscale.in_set(Core3dSystems::EarlyPostProcess),
        );
}
