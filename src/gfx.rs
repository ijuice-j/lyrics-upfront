//! GPU plumbing: Direct3D 11 device, a DirectComposition visual holding a
//! premultiplied-alpha swap chain, and a Direct2D context drawing into it.
//!
//! This is the efficient way to do a per-pixel transparent window on Windows:
//! DWM composites the swap chain directly, so no frame is ever copied back from
//! the GPU (the classic cost of layered windows). Frames are paced by the swap
//! chain's frame-latency waitable object, i.e. by the display's vsync.

use std::mem::ManuallyDrop;

use windows::core::{Interface, Result};
use windows::Win32::Foundation::{HANDLE, HMODULE, HWND};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectComposition::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;

pub struct Gpu {
    pub ctx: ID2D1DeviceContext,
    /// The swap chain's back buffer as a Direct2D target; restore after
    /// drawing into an offscreen layer.
    pub target: Option<ID2D1Bitmap1>,
    swap: IDXGISwapChain1,
    pub waitable: HANDLE,
    dcomp: IDCompositionDevice,
    _target: IDCompositionTarget,
    _visual: IDCompositionVisual,
    _d3d: ID3D11Device,
    pub width: u32,
    pub height: u32,
    pub dpi: f32,
}

const SWAP_FLAGS: DXGI_SWAP_CHAIN_FLAG = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT;

pub fn bitmap_props(dpi: f32, options: D2D1_BITMAP_OPTIONS) -> D2D1_BITMAP_PROPERTIES1 {
    D2D1_BITMAP_PROPERTIES1 {
        pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
        dpiX: dpi,
        dpiY: dpi,
        bitmapOptions: options,
        colorContext: ManuallyDrop::new(None),
    }
}

impl Gpu {
    pub fn new(hwnd: HWND, width: u32, height: u32, dpi: f32) -> Result<Self> {
        unsafe {
            let d3d = create_d3d()?;
            let dxgi: IDXGIDevice = d3d.cast()?;
            let factory: IDXGIFactory2 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: width.max(1),
                Height: height.max(1),
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
                AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
                Flags: SWAP_FLAGS.0 as u32,
                ..Default::default()
            };
            let swap = factory.CreateSwapChainForComposition(&dxgi, &desc, None)?;
            let swap2: IDXGISwapChain2 = swap.cast()?;
            swap2.SetMaximumFrameLatency(1)?;
            let waitable = swap2.GetFrameLatencyWaitableObject();

            let d2d_factory: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, Some(&D2D1_FACTORY_OPTIONS::default()))?;
            let device = d2d_factory.CreateDevice(&dxgi)?;
            let ctx = device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
            ctx.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE); // ClearType needs an opaque target

            let dcomp: IDCompositionDevice = DCompositionCreateDevice(&dxgi)?;
            let target = dcomp.CreateTargetForHwnd(hwnd, true)?;
            let visual = dcomp.CreateVisual()?;
            visual.SetContent(&swap)?;
            target.SetRoot(&visual)?;
            dcomp.Commit()?;

            let mut gpu = Gpu { ctx, target: None, swap, waitable, dcomp, _target: target, _visual: visual, _d3d: d3d, width, height, dpi };
            gpu.bind_target()?;
            Ok(gpu)
        }
    }

    unsafe fn bind_target(&mut self) -> Result<()> {
        let surface: IDXGISurface = self.swap.GetBuffer(0)?;
        let props = bitmap_props(self.dpi, D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW);
        let bitmap = self.ctx.CreateBitmapFromDxgiSurface(&surface, Some(&props))?;
        self.ctx.SetTarget(&bitmap);
        self.ctx.SetDpi(self.dpi, self.dpi);
        self.target = Some(bitmap);
        Ok(())
    }

    pub fn restore_target(&self) {
        unsafe {
            match &self.target {
                Some(t) => self.ctx.SetTarget(t),
                None => self.ctx.SetTarget(None::<&ID2D1Image>),
            }
        }
    }

    pub fn resize(&mut self, width: u32, height: u32, dpi: f32) -> Result<()> {
        if (width, height, dpi) == (self.width, self.height, self.dpi) {
            return Ok(());
        }
        unsafe {
            self.ctx.SetTarget(None);
            self.target = None;
            self.swap.ResizeBuffers(2, width.max(1), height.max(1), DXGI_FORMAT_B8G8R8A8_UNORM, SWAP_FLAGS)?;
            self.width = width;
            self.height = height;
            self.dpi = dpi;
            self.bind_target()
        }
    }

    /// Size of the drawing surface in DIPs.
    pub fn size_dips(&self) -> (f32, f32) {
        let scale = self.dpi / 96.0;
        (self.width as f32 / scale, self.height as f32 / scale)
    }

    /// An offscreen bitmap the size of the window that can be drawn into.
    pub fn layer_bitmap(&self) -> Result<ID2D1Bitmap1> {
        unsafe {
            let props = bitmap_props(self.dpi, D2D1_BITMAP_OPTIONS_TARGET);
            self.ctx.CreateBitmap(D2D_SIZE_U { width: self.width.max(1), height: self.height.max(1) }, None, 0, &props)
        }
    }

    /// An offscreen drawable bitmap of the given size in DIPs (at least 1px).
    pub fn bitmap(&self, w_dip: f32, h_dip: f32) -> Result<ID2D1Bitmap1> {
        let scale = self.dpi / 96.0;
        unsafe {
            let props = bitmap_props(self.dpi, D2D1_BITMAP_OPTIONS_TARGET);
            let size = D2D_SIZE_U { width: ((w_dip * scale).ceil() as u32).max(1), height: ((h_dip * scale).ceil() as u32).max(1) };
            self.ctx.CreateBitmap(size, None, 0, &props)
        }
    }

    /// The frame just drawn (before it is presented), as premultiplied BGRA
    /// rows: width, height, pixels. Used only for `LYRICS_UPFRONT_SNAPSHOT`.
    pub fn read_frame(&self) -> Result<(u32, u32, Vec<u8>)> {
        unsafe {
            let target = self.target.as_ref().ok_or_else(windows::core::Error::empty)?;
            let props = bitmap_props(self.dpi, D2D1_BITMAP_OPTIONS_CPU_READ | D2D1_BITMAP_OPTIONS_CANNOT_DRAW);
            let copy = self.ctx.CreateBitmap(D2D_SIZE_U { width: self.width, height: self.height }, None, 0, &props)?;
            copy.CopyFromBitmap(None, target, None)?;
            let mapped = copy.Map(D2D1_MAP_OPTIONS_READ)?;
            let row = self.width as usize * 4;
            let mut out = Vec::with_capacity(row * self.height as usize);
            for y in 0..self.height as usize {
                out.extend_from_slice(std::slice::from_raw_parts(mapped.bits.add(y * mapped.pitch as usize), row));
            }
            copy.Unmap()?;
            Ok((self.width, self.height, out))
        }
    }

    pub fn present(&self) -> Result<()> {
        unsafe { self.swap.Present(1, DXGI_PRESENT(0)).ok() }
    }

    #[allow(dead_code)]
    pub fn commit(&self) -> Result<()> {
        unsafe { self.dcomp.Commit() }
    }
}

unsafe fn create_d3d() -> Result<ID3D11Device> {
    let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_10_0];
    // Experiment switch: LYRICS_UPFRONT_WARP=1 renders with Windows' software
    // rasterizer instead of the GPU driver (less driver memory, more CPU).
    let drivers = if std::env::var_os("LYRICS_UPFRONT_WARP").is_some() {
        [D3D_DRIVER_TYPE_WARP, D3D_DRIVER_TYPE_HARDWARE]
    } else {
        [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP]
    };
    for driver in drivers {
        let mut device = None;
        if D3D11CreateDevice(
            None,
            driver,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
        .is_ok()
        {
            if let Some(d) = device {
                return Ok(d);
            }
        }
    }
    Err(windows::core::Error::empty())
}
