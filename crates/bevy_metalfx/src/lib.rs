//! MetalFX temporal upscaling for Bevy, as a drop-in camera component.
//!
//! Add [`MetalFxTemporalUpscaling`] to a perspective 3D camera and the main pass renders at
//! `viewport / factor`, then `MTLFXTemporalScaler` reconstructs the full-resolution image during
//! [`Core3dSystems::EarlyPostProcess`](bevy::core_pipeline::Core3dSystems::EarlyPostProcess).
//!
//! Everything is a no-op on non-macOS targets, so the component can be inserted unconditionally.

use bevy::prelude::*;

#[cfg(target_os = "macos")]
mod macos;

/// Registers MetalFX render-world support. Insert alongside `DefaultPlugins`.
#[derive(Default)]
pub struct MetalFxPlugin;

impl Plugin for MetalFxPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<MetalFxTemporalUpscaling>()
            .register_type::<UpscaleFactor>();

        #[cfg(target_os = "macos")]
        macos::build(app);
    }
}

/// Ratio between a camera's output resolution and the resolution its 3D main pass renders at.
///
/// Values outside the range MetalFX accepts are not representable, so the render code never has to
/// defend against a zero, negative, NaN, or downscaling ratio.
#[derive(Reflect, Clone, Copy, PartialEq, PartialOrd, Debug)]
pub struct UpscaleFactor(f32);

impl UpscaleFactor {
    /// 1.0 renders at native resolution and uses MetalFX purely as a temporal antialiaser.
    pub const NATIVE: Self = Self(1.0);
    pub const MIN: f32 = 1.0;
    /// `MTLFXTemporalScaler`'s documented ceiling. The device-reported limit is applied on top.
    pub const MAX: f32 = 3.0;

    pub fn new(factor: f32) -> Option<Self> {
        (factor.is_finite() && (Self::MIN..=Self::MAX).contains(&factor)).then_some(Self(factor))
    }

    /// Clamps into the supported range, mapping non-finite input to [`Self::NATIVE`].
    pub fn clamped(factor: f32) -> Self {
        if factor.is_finite() {
            Self(factor.clamp(Self::MIN, Self::MAX))
        } else {
            Self::NATIVE
        }
    }

    pub fn get(self) -> f32 {
        self.0
    }
}

impl Default for UpscaleFactor {
    fn default() -> Self {
        Self(2.0)
    }
}

/// Enables MetalFX temporal upscaling on a perspective 3D camera.
///
/// The camera must cover its entire render target (no sub-viewport): MetalFX reads its inputs from
/// the target's top-left corner, which is also where the reduced-resolution main pass renders.
#[derive(Component, Reflect, Clone, Copy, Default, Debug)]
#[reflect(Component, Default, Clone)]
#[require(
    bevy::render::camera::TemporalJitter,
    bevy::render::camera::MipBias,
    bevy::core_pipeline::prepass::DepthPrepass,
    bevy::core_pipeline::prepass::MotionVectorPrepass
)]
pub struct MetalFxTemporalUpscaling {
    pub factor: UpscaleFactor,
    /// Runs `MTLFXFrameInterpolator` after upscaling. Requires macOS 26 or newer and a supported
    /// GPU; on anything else the flag is reported once and ignored.
    pub frame_generation: bool,
}

/// Drops the accumulated temporal history on the next frame, then removes itself.
///
/// Insert after a camera cut (view switch, teleport) so the reconstruction doesn't smear the old
/// view into the new one. History is reset automatically whenever the scaler is (re)created, so
/// this is only needed for cuts that keep the same resolution and format.
#[derive(Component, Reflect, Clone, Copy, Default, Debug)]
#[reflect(Component, Default, Clone)]
pub struct MetalFxResetHistory;
