// Added 2026-11-21, not in upstream gpui-ce: custom GPU controls on Windows
// (branch `windows-custom-gpu`).

//! Custom GPU controls on Windows: the producer's side of gpui's
//! `WindowsSharedSurface` (its module doc says how the two sides take turns).
//!
//! A Windows window draws with Direct3D 11, so it has no wgpu device to share
//! as macOS's and Linux's do. [`WgpuContextHandle::from_window`] gives instead
//! a wgpu Direct3D 12 device of this process on the window's adapter, one per
//! adapter for every window on it ([`context_for`]), and a [`WgpuRenderTarget`]
//! made with it is a texture shared with the window's renderer: its surface is
//! composited with no copy, its use ordered on the GPU by two fences the device
//! shares with every window ([`Exchange`]).
//!
//! The device takes gpui's own requirements: `App::set_gpu_requirements` does
//! not reach it, as the Windows platform does not keep them.
//!
//! [`WgpuContextHandle::from_window`]: crate::WgpuContextHandle::from_window
//! [`WgpuRenderTarget`]: crate::WgpuRenderTarget

use crate::{WgpuContext, WgpuContextHandle};
use anyhow::{Context as _, Result};
use gpui_util::ResultExt as _;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::GENERIC_ALL;
use windows::Win32::Graphics::Direct3D12::{
    D3D12_FENCE_FLAG_SHARED, D3D12_HEAP_FLAG_SHARED, D3D12_HEAP_PROPERTIES,
    D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_DESC, D3D12_RESOURCE_DIMENSION_TEXTURE2D,
    D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET, D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS,
    D3D12_RESOURCE_STATE_COMMON, D3D12_TEXTURE_LAYOUT_UNKNOWN, ID3D12CommandQueue, ID3D12Device,
    ID3D12Fence, ID3D12Resource,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
    DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM_SRGB, DXGI_SAMPLE_DESC,
};

/// The texture format a window's shared surfaces take: what gpui's Direct3D 11
/// renderer draws with.
pub(crate) const TEXTURE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8Unorm;

/// A device of this process on one adapter, with what it shares with the
/// windows drawn there.
struct AdapterDevice {
    luid: i64,
    context: WgpuContext,
    exchange: Arc<Exchange>,
}

/// One device per adapter, for every window on it: a control moved from one
/// window to another keeps its device and its resources.
static DEVICES: Mutex<Vec<AdapterDevice>> = Mutex::new(Vec::new());

/// The context for a window drawn on `adapter`: this process's device there,
/// made the first time it is asked for and again once it is lost.
pub(crate) fn context_for(adapter: gpui::WindowsGpuAdapter) -> Option<WgpuContextHandle> {
    let mut devices = DEVICES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    devices.retain(|d| !d.context.device_lost());
    if let Some(device) = devices.iter().find(|d| d.luid == adapter.luid) {
        return Some(handle(device));
    }
    let made = make_device(adapter.luid)
        .context("making a Direct3D 12 device for a custom GPU control")
        .log_err()?;
    let handle = handle(&made);
    devices.push(made);
    Some(handle)
}

fn handle(device: &AdapterDevice) -> WgpuContextHandle {
    let mut handle = device.context.handle(TEXTURE_FORMAT);
    handle.windows = Some(Arc::clone(&device.exchange));
    handle
}

fn make_device(luid: i64) -> Result<AdapterDevice> {
    let context = WgpuContext::new_on_dx12_adapter(luid)?;
    let exchange = Exchange::new(&context)?;
    Ok(AdapterDevice {
        luid,
        context,
        exchange: Arc::new(exchange),
    })
}

/// The adapter's locally unique id, as `gpui::WindowsGpuAdapter` has it.
pub(crate) fn adapter_luid(adapter: &wgpu::Adapter) -> Option<i64> {
    // SAFETY: the hal adapter is only read, while `adapter` is alive.
    let hal = unsafe { adapter.as_hal::<wgpu::hal::api::Dx12>() }?;
    // SAFETY: a plain query of the adapter's description.
    let desc = unsafe { hal.raw_adapter().GetDesc1() }.ok()?;
    let luid = desc.AdapterLuid;
    Some(((luid.HighPart as i64) << 32) | luid.LowPart as i64)
}

fn raw_device(context: &WgpuContext) -> Result<(ID3D12Device, ID3D12CommandQueue)> {
    // SAFETY: the raw objects are cloned (AddRef) out of the hal device, which
    // outlives nothing they are used for.
    let hal = unsafe { context.device.as_hal::<wgpu::hal::api::Dx12>() }
        .context("the device is not Direct3D 12")?;
    Ok((hal.raw_device().clone(), hal.raw_queue().clone()))
}

/// The two fences a device shares with every window it draws into, and the
/// values it has signalled and been promised (gpui's `WindowsSharedSurface`).
pub(crate) struct Exchange {
    queue: ID3D12CommandQueue,
    produced: ID3D12Fence,
    consumed: ID3D12Fence,
    produced_handle: Arc<gpui::WindowsSharedHandle>,
    consumed_handle: Arc<gpui::WindowsSharedHandle>,
    /// The last value `produced` was signalled with.
    produced_value: AtomicU64,
    /// The last value a window's renderer said it signals `consumed` with:
    /// the producer's next write waits for it.
    consumed_value: AtomicU64,
    device_lost: Arc<AtomicBool>,
}

impl Exchange {
    fn new(context: &WgpuContext) -> Result<Self> {
        let (device, queue) = raw_device(context)?;
        let fence = || -> Result<(ID3D12Fence, Arc<gpui::WindowsSharedHandle>)> {
            // SAFETY: plain Direct3D 12 object creation on a live device.
            let fence: ID3D12Fence = unsafe { device.CreateFence(0, D3D12_FENCE_FLAG_SHARED) }
                .context("creating a shared fence")?;
            let handle = unsafe { device.CreateSharedHandle(&fence, None, GENERIC_ALL.0, None) }
                .context("sharing a fence")?;
            // SAFETY: the handle was just made and is owned by nothing else.
            Ok((
                fence,
                Arc::new(unsafe { gpui::WindowsSharedHandle::from_raw(handle.0) }),
            ))
        };
        let (produced, produced_handle) = fence()?;
        let (consumed, consumed_handle) = fence()?;
        Ok(Self {
            queue,
            produced,
            consumed,
            produced_handle,
            consumed_handle,
            produced_value: AtomicU64::new(0),
            consumed_value: AtomicU64::new(0),
            device_lost: context.device_lost_flag(),
        })
    }

    /// Before the producer writes a shared texture again: its queue waits until
    /// every window's read promised so far is done.
    pub(crate) fn before_write(&self) {
        let value = self.consumed_value.load(Ordering::Acquire);
        if value == 0 {
            return;
        }
        // SAFETY: a queue wait on a live fence; the queue is free-threaded.
        if let Err(error) = unsafe { self.queue.Wait(&self.consumed, value) } {
            log::error!("waiting for a window's read of a shared texture: {error}");
        }
    }
}

impl gpui::WindowsSurfaceSync for Exchange {
    fn begin_read(&self) -> Option<u64> {
        if self.device_lost.load(Ordering::Relaxed) {
            return None;
        }
        let value = self.produced_value.fetch_add(1, Ordering::AcqRel) + 1;
        // SAFETY: a queue signal on a live fence; the queue is free-threaded.
        unsafe { self.queue.Signal(&self.produced, value) }
            .context("signalling a shared texture's producer fence")
            .log_err()?;
        Some(value)
    }

    fn end_read(&self) -> u64 {
        self.consumed_value.fetch_add(1, Ordering::AcqRel) + 1
    }

    fn abandon(&self) {
        // the fence set from here to what was promised, so no wait for it hangs
        let value = self.consumed_value.load(Ordering::Acquire);
        // SAFETY: a CPU-side signal of a live fence.
        if let Err(error) = unsafe { self.consumed.Signal(value) } {
            log::error!("releasing a shared texture's reads: {error}");
        }
    }
}

/// A render target's texture, shared with the windows on its device's adapter,
/// and its surface for them.
pub(crate) struct SharedTarget {
    pub(crate) surface: gpui::WindowsSharedSurface,
    pub(crate) exchange: Arc<Exchange>,
}

/// The DXGI format a shareable texture of `format` is made with, if it can be:
/// one Direct3D 11 can open and sample.
fn dxgi_format(format: wgpu::TextureFormat) -> Option<DXGI_FORMAT> {
    Some(match format {
        wgpu::TextureFormat::Bgra8Unorm => DXGI_FORMAT_B8G8R8A8_UNORM,
        wgpu::TextureFormat::Bgra8UnormSrgb => DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
        wgpu::TextureFormat::Rgba8Unorm => DXGI_FORMAT_R8G8B8A8_UNORM,
        wgpu::TextureFormat::Rgba8UnormSrgb => DXGI_FORMAT_R8G8B8A8_UNORM_SRGB,
        _ => return None,
    })
}

/// A texture of `size` and `format` shared with the windows on the device's
/// adapter: a Direct3D 12 resource made shareable (simultaneous access, so it
/// is in the common state between command lists, which Direct3D 11 reads it in)
/// and given to wgpu.
pub(crate) fn create_target(
    context: &WgpuContextHandle,
    exchange: &Arc<Exchange>,
    size: gpui::Size<gpui::DevicePixels>,
    format: wgpu::TextureFormat,
    usage: wgpu::TextureUsages,
) -> Result<(wgpu::Texture, SharedTarget)> {
    let dxgi = dxgi_format(format)
        .with_context(|| format!("{format:?} cannot be shared with a window"))?;
    // SAFETY: as in `raw_device`.
    let hal = unsafe { context.device().as_hal::<wgpu::hal::api::Dx12>() }
        .context("the device is not Direct3D 12")?;
    let device = hal.raw_device().clone();
    let (width, height) = (size.width.0 as u32, size.height.0 as u32);
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        ..Default::default()
    };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Alignment: 0,
        Width: width as u64,
        Height: height,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: dxgi,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
        Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET
            | D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS,
    };
    let mut resource: Option<ID3D12Resource> = None;
    // SAFETY: plain resource creation on a live device, the descriptions above.
    unsafe {
        device.CreateCommittedResource(
            &heap,
            D3D12_HEAP_FLAG_SHARED,
            &desc,
            D3D12_RESOURCE_STATE_COMMON,
            None,
            &mut resource,
        )
    }
    .context("creating a shared texture")?;
    let resource = resource.context("Direct3D 12 gave no shared texture")?;
    // SAFETY: as for the fences.
    let handle = unsafe { device.CreateSharedHandle(&resource, None, GENERIC_ALL.0, None) }
        .context("sharing a texture")?;
    // SAFETY: the handle was just made and is owned by nothing else.
    let handle = Arc::new(unsafe { gpui::WindowsSharedHandle::from_raw(handle.0) });
    drop(hal);

    let extent = wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    };
    // SAFETY: the resource is a 2D texture of this format and size, one mip and
    // one sample, and wgpu owns it from here.
    let hal_texture = unsafe {
        wgpu::hal::dx12::Device::texture_from_raw(
            resource,
            format,
            wgpu::TextureDimension::D2,
            extent,
            1,
            1,
        )
    };
    let texture = unsafe {
        context
            .device()
            .create_texture_from_hal::<wgpu::hal::api::Dx12>(
                hal_texture,
                &wgpu::TextureDescriptor {
                    label: Some("gpui-custom-render-target-shared"),
                    size: extent,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage,
                    view_formats: &[],
                },
            )
    };
    let sync: Arc<dyn gpui::WindowsSurfaceSync> = Arc::clone(exchange) as _;
    let surface = gpui::WindowsSharedSurface::new(
        size,
        handle,
        Arc::clone(&exchange.produced_handle),
        Arc::clone(&exchange.consumed_handle),
        sync,
    );
    Ok((
        texture,
        SharedTarget {
            surface,
            exchange: Arc::clone(exchange),
        },
    ))
}
