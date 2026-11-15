//! The app icon: the Lyrics UpFront mark (assets/logo.png), pre-sized into two
//! .ico files by assets/make_icons.py. Pure Rust with no Windows calls, so
//! build.rs can include this file to embed the icons as exe resources, and the
//! app can use it for `--write-icon` and for the idle card.
//!
//! Resource ids: icon group 1 is the mark (exe, shortcuts, Settings window,
//! tray while playing); group 2 is the greyed mark (tray while paused or idle).

/// The mark, at every size in `make_icons.py`.
pub const ICON: &[u8] = include_bytes!("../assets/app.ico");
/// The greyed mark. Only build.rs reads it; the app loads it from resources.
#[allow(dead_code)]
pub const ICON_PAUSED: &[u8] = include_bytes!("../assets/app-paused.ico");

pub const GROUP_ICON: u16 = 1;
pub const GROUP_ICON_PAUSED: u16 = 2;

/// One image of an .ico file: its pixel size and its data (PNG or DIB).
pub struct Image<'a> {
    pub size: u32,
    pub data: &'a [u8],
}

/// The images in an .ico file, in file order.
pub fn images(ico: &[u8]) -> Vec<Image<'_>> {
    let u16_at = |at: usize| u16::from_le_bytes([ico[at], ico[at + 1]]) as usize;
    let u32_at = |at: usize| u32::from_le_bytes([ico[at], ico[at + 1], ico[at + 2], ico[at + 3]]) as usize;
    let count = u16_at(4);
    (0..count)
        .map(|i| {
            let entry = 6 + 16 * i;
            let size = if ico[entry] == 0 { 256 } else { ico[entry] as u32 };
            let (len, offset) = (u32_at(entry + 8), u32_at(entry + 12));
            Image { size, data: &ico[offset..offset + len] }
        })
        .collect()
}

/// The largest image of the mark (a 256px PNG), for drawing it in the app.
pub fn largest() -> &'static [u8] {
    images(ICON).into_iter().max_by_key(|i| i.size).map(|i| i.data).unwrap_or(&[])
}

/// The RT_GROUP_ICON resource that ties RT_ICON ids `first_id..` together.
/// Used by build.rs.
#[allow(dead_code)]
pub fn group_icon(images: &[Image], first_id: u16) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // type: icon
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());
    for (i, image) in images.iter().enumerate() {
        let side = if image.size >= 256 { 0 } else { image.size as u8 };
        out.push(side);
        out.push(side);
        out.extend_from_slice(&[0, 0]); // colour count, reserved
        out.extend_from_slice(&1u16.to_le_bytes()); // planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        out.extend_from_slice(&(image.data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(first_id + i as u16).to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icons_parse_at_every_size() {
        for ico in [ICON, ICON_PAUSED] {
            let sizes: Vec<u32> = images(ico).iter().map(|i| i.size).collect();
            assert_eq!(sizes.len(), 9);
            for want in [16, 20, 24, 32, 40, 48, 64, 128, 256] {
                assert!(sizes.contains(&want), "missing {want}px");
            }
        }
        assert!(largest().starts_with(b"\x89PNG"));
    }
}
