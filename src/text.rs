//! DirectWrite: the overlay's own variable font (Mona Sans, from assets/fonts),
//! loaded from memory, with exact weights such as 650 and 760 via the weight
//! axis. Falls back to Segoe UI if the font cannot be loaded.

use windows::core::{w, Interface, Result};
use windows::Win32::Graphics::DirectWrite::*;

const MONA_SANS: &[u8] = include_bytes!("../assets/fonts/MonaSans-latin.woff2");

pub struct Text {
    pub factory: IDWriteFactory,
    variable: Option<(IDWriteFactory6, IDWriteFontCollection2)>,
}

fn tag(s: &[u8; 4]) -> DWRITE_FONT_AXIS_TAG {
    DWRITE_FONT_AXIS_TAG(u32::from_le_bytes(*s))
}

impl Text {
    pub fn new() -> Result<Self> {
        let factory: IDWriteFactory = unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)? };
        let variable = unsafe { Self::load_mona(&factory).ok() };
        Ok(Text { factory, variable })
    }

    unsafe fn load_mona(factory: &IDWriteFactory) -> Result<(IDWriteFactory6, IDWriteFontCollection2)> {
        let f5: IDWriteFactory5 = factory.cast()?;
        let f6: IDWriteFactory6 = factory.cast()?;
        // WOFF2 -> OpenType in memory (DirectWrite does the decompression).
        let stream = f5.UnpackFontFile(DWRITE_CONTAINER_TYPE_WOFF2, MONA_SANS.as_ptr() as _, MONA_SANS.len() as u32)?;
        let size = stream.GetFileSize()?;
        let mut ptr = std::ptr::null_mut();
        let mut ctx = std::ptr::null_mut();
        stream.ReadFileFragment(&mut ptr, 0, size, &mut ctx)?;
        let data = std::slice::from_raw_parts(ptr as *const u8, size as usize).to_vec();
        stream.ReleaseFileFragment(ctx);

        let loader = f5.CreateInMemoryFontFileLoader()?;
        f5.RegisterFontFileLoader(&loader)?;
        let file = loader.CreateInMemoryFontFileReference(&f5, data.as_ptr() as _, data.len() as u32, None)?;
        let builder = f5.CreateFontSetBuilder()?;
        builder.AddFontFile(&file)?;
        let set = builder.CreateFontSet()?;
        let collection = f6.CreateFontCollectionFromFontSet(&set, DWRITE_FONT_FAMILY_MODEL_TYPOGRAPHIC)?;
        let (mut index, mut exists) = (0u32, Default::default());
        collection.FindFamilyName(w!("Mona Sans"), &mut index, &mut exists)?;
        if !exists.as_bool() {
            return Err(windows::core::Error::empty());
        }
        Ok((f6, collection))
    }


    /// A text format at `size` DIPs and an exact `weight` (100-900, fractional ok).
    pub fn format(&self, size: f32, weight: f32) -> Result<IDWriteTextFormat> {
        unsafe {
            if let Some((f6, collection)) = &self.variable {
                let axes = [
                    DWRITE_FONT_AXIS_VALUE { axisTag: tag(b"wght"), value: weight },
                    DWRITE_FONT_AXIS_VALUE { axisTag: tag(b"wdth"), value: 100.0 },
                ];
                if let Ok(f) = f6.CreateTextFormat(w!("Mona Sans"), collection, &axes, size, w!("en-us")) {
                    return f.cast();
                }
            }
            self.factory.CreateTextFormat(
                w!("Segoe UI"),
                None,
                DWRITE_FONT_WEIGHT(weight.round() as i32),
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                size,
                w!("en-us"),
            )
        }
    }

    /// Lay out `text`. `wrap` false = single line trimmed with an ellipsis.
    pub fn layout(&self, text: &str, format: &IDWriteTextFormat, max_w: f32, max_h: f32, wrap: bool) -> Result<IDWriteTextLayout> {
        unsafe {
            format.SetWordWrapping(if wrap { DWRITE_WORD_WRAPPING_WRAP } else { DWRITE_WORD_WRAPPING_NO_WRAP })?;
            let sign = self.factory.CreateEllipsisTrimmingSign(format)?;
            let trimming = DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER, delimiter: 0, delimiterCount: 0 };
            format.SetTrimming(&trimming, &sign)?;
            let wide: Vec<u16> = text.encode_utf16().collect();
            self.factory.CreateTextLayout(&wide, format, max_w.max(1.0), max_h.max(1.0))
        }
    }

    /// Letter spacing in DIPs (CSS letter-spacing) over the whole layout.
    pub fn letter_spacing(layout: &IDWriteTextLayout, len: usize, spacing: f32) {
        if let Ok(l1) = layout.cast::<IDWriteTextLayout1>() {
            unsafe {
                let _ = l1.SetCharacterSpacing(0.0, spacing, 0.0, DWRITE_TEXT_RANGE { startPosition: 0, length: len as u32 });
            }
        }
    }

    /// Tabular figures, so the clock does not jitter as digits change.
    pub fn tabular(&self, layout: &IDWriteTextLayout, len: usize) {
        unsafe {
            if let Ok(t) = self.factory.CreateTypography() {
                let _ = t.AddFontFeature(DWRITE_FONT_FEATURE { nameTag: DWRITE_FONT_FEATURE_TAG_TABULAR_FIGURES, parameter: 1 });
                let _ = layout.SetTypography(&t, DWRITE_TEXT_RANGE { startPosition: 0, length: len as u32 });
            }
        }
    }
}

