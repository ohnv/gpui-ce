// Changed 2026-11-21, not in upstream gpui-ce: custom GPU controls on Windows
// (branch `windows-custom-gpu`): the windows_shared module.

mod wgpu_atlas;
mod wgpu_context;
#[cfg(all(target_os = "windows", feature = "custom-gpu"))]
mod windows_shared;
mod wgpu_renderer;

pub use wgpu;
pub use wgpu_atlas::*;
pub use wgpu_context::*;
#[cfg(all(feature = "test-support", not(target_family = "wasm")))]
pub use wgpu_renderer::WgpuHeadlessRenderer;
pub use wgpu_renderer::{
    FontRasterizationSettings, GpuContext, SubpixelOrder, WgpuRenderer, WgpuSurfaceConfig,
};
