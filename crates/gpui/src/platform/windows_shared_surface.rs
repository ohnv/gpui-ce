// Added 2026-11-21, not in upstream gpui-ce: custom GPU controls on Windows
// (branch `windows-custom-gpu`).

//! A texture rendered on another Direct3D 12 device and shared with a window's
//! Direct3D 11 renderer, which is how a custom GPU control draws into a window
//! on Windows without a copy.
//!
//! On macOS and Linux a custom control renders on the window's own wgpu device,
//! and its queue orders the control's writes and the window's reads. On Windows
//! the window draws with Direct3D 11 and the control with wgpu's Direct3D 12, on
//! the same adapter ([`WindowsGpuAdapter`]): the texture and two fences are
//! shared between the devices by NT handle, and they take turns with the texture
//! on the GPU, neither waiting on the CPU:
//!
//! 1. the producer's queue signals `produced` after the work that wrote the
//!    texture, and the renderer waits for that value before reading it;
//! 2. the renderer signals `consumed` after its read, and the producer's queue
//!    waits for that value before it writes the texture again.

use crate::{DevicePixels, Size};
use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// The DXGI adapter a window draws with, as [`crate::Window::gpu_context_info`]
/// gives it on Windows. A custom GPU control makes its device on the same
/// adapter: only a device on the window's adapter can share textures with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WindowsGpuAdapter {
    /// The adapter's locally unique id (`DXGI_ADAPTER_DESC1::AdapterLuid`):
    /// its high part in the upper 32 bits, its low part in the lower.
    pub luid: i64,
}

/// How a shared surface's producer and the window's renderer take turns with
/// it (the module's two steps). The producer implements it; the renderer calls
/// it as it draws the surface.
pub trait WindowsSurfaceSync: Send + Sync {
    /// The renderer is about to read the texture: the producer's queue signals
    /// `produced` after everything submitted to it so far, and this is the value
    /// to wait for. `None` when the producer cannot signal (its device was
    /// lost): the surface is not drawn this frame.
    fn begin_read(&self) -> Option<u64>;

    /// The value the renderer signals `consumed` with once its read is done.
    fn end_read(&self) -> u64;

    /// The renderer lost its device, so a value it was given by
    /// [`Self::end_read`] may never be signalled: the producer must stop
    /// waiting for it.
    fn abandon(&self);
}

/// An NT handle to a shared Direct3D object, closed when dropped.
pub struct WindowsSharedHandle(*mut c_void);

// SAFETY: an NT handle is a process-wide value, usable from any thread.
unsafe impl Send for WindowsSharedHandle {}
// SAFETY: as above; the handle is only read.
unsafe impl Sync for WindowsSharedHandle {}

impl WindowsSharedHandle {
    /// Takes ownership of `handle`.
    ///
    /// # Safety
    /// `handle` is an open NT handle that nothing else closes.
    pub unsafe fn from_raw(handle: *mut c_void) -> Self {
        Self(handle)
    }

    /// The handle, for opening the object it names. It stays owned here.
    pub fn as_raw(&self) -> *mut c_void {
        self.0
    }
}

impl Drop for WindowsSharedHandle {
    fn drop(&mut self) {
        // SAFETY: the handle is owned here and closed only here.
        if let Err(error) = unsafe {
            windows::Win32::Foundation::CloseHandle(windows::Win32::Foundation::HANDLE(self.0))
        } {
            log::error!("failed to close a shared surface handle: {error}");
        }
    }
}

/// A texture shared with the window's renderer, with the fences that order its
/// use (the module's doc). Clones are the same surface.
#[derive(Clone)]
pub struct WindowsSharedSurface {
    id: u64,
    size: Size<DevicePixels>,
    texture: Arc<WindowsSharedHandle>,
    produced: Arc<WindowsSharedHandle>,
    consumed: Arc<WindowsSharedHandle>,
    sync: Arc<dyn WindowsSurfaceSync>,
}

impl WindowsSharedSurface {
    /// A surface of a BGRA8 texture of `size`, shared by `texture`, with the
    /// fences `produced` and `consumed`. Each call is a surface of its own,
    /// which the renderer opens once and keeps while it is drawn.
    pub fn new(
        size: Size<DevicePixels>,
        texture: Arc<WindowsSharedHandle>,
        produced: Arc<WindowsSharedHandle>,
        consumed: Arc<WindowsSharedHandle>,
        sync: Arc<dyn WindowsSurfaceSync>,
    ) -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            size,
            texture,
            produced,
            consumed,
            sync,
        }
    }

    /// What the renderer keeps the opened texture by.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The texture's size in device pixels.
    pub fn size(&self) -> Size<DevicePixels> {
        self.size
    }

    /// The shared texture's handle.
    pub fn texture_handle(&self) -> *mut c_void {
        self.texture.as_raw()
    }

    /// The `produced` fence's handle.
    pub fn produced_handle(&self) -> *mut c_void {
        self.produced.as_raw()
    }

    /// The `consumed` fence's handle.
    pub fn consumed_handle(&self) -> *mut c_void {
        self.consumed.as_raw()
    }

    /// How the producer and the renderer take turns with the texture.
    pub fn sync(&self) -> &Arc<dyn WindowsSurfaceSync> {
        &self.sync
    }
}

impl std::fmt::Debug for WindowsSharedSurface {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WindowsSharedSurface")
            .field("id", &self.id)
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}
