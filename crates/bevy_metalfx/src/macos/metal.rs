//! Thin helpers over the wgpu/Metal boundary.

use bevy::math::UVec2;
use bevy::render::render_resource::{Texture, TextureFormat};
use objc2::{Message, rc::Retained, runtime::AnyClass, runtime::ProtocolObject};
use objc2_metal::{MTLPixelFormat, MTLTexture};
use std::ffi::CStr;

/// Keeps the underlying `MTLTexture` alive for as long as MetalFX needs the reference.
pub(super) fn retain_metal_texture(
    texture: &Texture,
) -> Option<Retained<ProtocolObject<dyn MTLTexture>>> {
    let texture = unsafe { texture.as_hal::<wgpu::hal::api::Metal>() }?;
    Some(texture.raw_handle().retain())
}

/// MetalFX only accepts textures whose pixel format it was configured with, so an unmapped format
/// means "don't create a scaler" rather than "guess".
pub(super) fn metal_pixel_format(format: TextureFormat) -> Option<MTLPixelFormat> {
    match format {
        TextureFormat::Rgba8Unorm => Some(MTLPixelFormat::RGBA8Unorm),
        TextureFormat::Rgba8UnormSrgb => Some(MTLPixelFormat::RGBA8Unorm_sRGB),
        TextureFormat::Bgra8Unorm => Some(MTLPixelFormat::BGRA8Unorm),
        TextureFormat::Bgra8UnormSrgb => Some(MTLPixelFormat::BGRA8Unorm_sRGB),
        TextureFormat::Rgba16Float => Some(MTLPixelFormat::RGBA16Float),
        TextureFormat::Rg16Float => Some(MTLPixelFormat::RG16Float),
        TextureFormat::Depth32Float => Some(MTLPixelFormat::Depth32Float),
        _ => None,
    }
}

/// The MetalFX classes are weakly linked; older systems simply don't have them.
pub(super) fn objc_class_exists(name: &'static [u8]) -> bool {
    let name =
        CStr::from_bytes_with_nul(name).expect("Objective-C class name must be NUL-terminated");
    AnyClass::get(name).is_some()
}

pub(super) fn texture_size(texture: &Texture) -> UVec2 {
    UVec2::new(texture.width(), texture.height())
}
