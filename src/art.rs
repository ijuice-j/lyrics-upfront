//! Album art: decode with Windows Imaging Component (built into Windows), then
//! produce a 128px cover thumbnail and the 64px sample the palette reads.
//! Runs on a worker thread so a track change never stalls the frame loop.

use windows::core::{Interface, Result, GUID};
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::*;
use windows::Win32::UI::Shell::SHCreateMemStream;

use crate::palette::{self, Theme};

pub const COVER_PX: u32 = 128;

pub struct Cover {
    pub width: u32,
    pub height: u32,
    /// Premultiplied BGRA, top-down, stride = width * 4.
    pub pbgra: Vec<u8>,
}

pub struct Decoded {
    pub theme: Theme,
    pub cover: Cover,
}

pub fn decode(bytes: &[u8], tint: bool) -> Option<Decoded> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        decode_inner(bytes, tint).ok()
    }
}

unsafe fn decode_inner(bytes: &[u8], tint: bool) -> Result<Decoded> {
    let factory: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
    let stream = SHCreateMemStream(Some(bytes)).ok_or_else(windows::core::Error::empty)?;
    let decoder = factory.CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)?;
    let frame = decoder.GetFrame(0)?;
    let (mut w, mut h) = (0u32, 0u32);
    frame.GetSize(&mut w, &mut h)?;
    if w == 0 || h == 0 {
        return Err(windows::core::Error::empty());
    }

    // Palette sample: 64x64, flattened onto black (premultiplied == composited on black).
    let theme = if tint {
        let px = scaled_pixels(&factory, &frame.cast()?, 64, 64)?;
        let rgb: Vec<[u8; 3]> = px.chunks_exact(4).map(|p| [p[2], p[1], p[0]]).collect();
        palette::theme_from_rgb(&rgb)
    } else {
        palette::NEUTRAL
    };

    // Cover: scale so the short side is COVER_PX, then crop the centre square.
    let scale = COVER_PX as f64 / w.min(h) as f64;
    let (sw, sh) = (((w as f64 * scale).round() as u32).max(COVER_PX), ((h as f64 * scale).round() as u32).max(COVER_PX));
    let scaler = factory.CreateBitmapScaler()?;
    scaler.Initialize(&frame, sw, sh, WICBitmapInterpolationModeHighQualityCubic)?;
    let clipper = factory.CreateBitmapClipper()?;
    let rect = WICRect { X: ((sw - COVER_PX) / 2) as i32, Y: ((sh - COVER_PX) / 2) as i32, Width: COVER_PX as i32, Height: COVER_PX as i32 };
    clipper.Initialize(&scaler, &rect)?;
    let pbgra = convert_pixels(&factory, &clipper.cast()?, COVER_PX, COVER_PX)?;
    Ok(Decoded { theme, cover: Cover { width: COVER_PX, height: COVER_PX, pbgra } })
}

unsafe fn scaled_pixels(factory: &IWICImagingFactory, source: &IWICBitmapSource, w: u32, h: u32) -> Result<Vec<u8>> {
    let scaler = factory.CreateBitmapScaler()?;
    scaler.Initialize(source, w, h, WICBitmapInterpolationModeFant)?;
    convert_pixels(factory, &scaler.cast()?, w, h)
}

unsafe fn convert_pixels(factory: &IWICImagingFactory, source: &IWICBitmapSource, w: u32, h: u32) -> Result<Vec<u8>> {
    let converter = factory.CreateFormatConverter()?;
    let format: GUID = GUID_WICPixelFormat32bppPBGRA;
    converter.Initialize(source, &format, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom)?;
    let mut buf = vec![0u8; (w * h * 4) as usize];
    converter.CopyPixels(std::ptr::null(), w * 4, &mut buf)?;
    Ok(buf)
}

/// Solid-colour cover for the simulator, without going through an encoder.
pub fn solid(rgb: [u8; 3], tint: bool) -> Decoded {
    let theme = if tint { palette::theme_from_rgb(&vec![rgb; 64 * 64]) } else { palette::NEUTRAL };
    let px = [rgb[2], rgb[1], rgb[0], 255];
    let pbgra = px.repeat((COVER_PX * COVER_PX) as usize);
    Decoded { theme, cover: Cover { width: COVER_PX, height: COVER_PX, pbgra } }
}
