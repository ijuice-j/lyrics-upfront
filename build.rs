//! Embeds the app icon and the application manifest into the exe.
//!
//! Windows resources normally need a resource compiler (rc.exe or windres),
//! which neither the GNU Rust toolchain nor a bare CI image is guaranteed to
//! have. A compiled resource is only a COFF object with a `.rsrc` section, so
//! this writes that object directly and hands it to the linker. Both the GNU
//! linker and Microsoft's link.exe accept it.
//!
//! - RT_ICON / RT_GROUP_ICON 1: the Lyrics UpFront mark at several sizes
//!   (Explorer, shortcuts, the Settings window and the tray use it).
//!   RT_GROUP_ICON 2: the same mark greyed, for the tray while paused.
//! - RT_MANIFEST 1: Common Controls v6 (modern-looking Settings controls) and
//!   per-monitor-v2 DPI awareness.
//! - RT_VERSION 1: the app's name and version. Windows shows the description
//!   ("Lyrics UpFront") in Task Manager, Startup apps and the tray icon
//!   settings, instead of the exe's file name.

#[path = "src/icon_art.rs"]
#[allow(dead_code)]
mod icon_art;

use std::path::PathBuf;

const RT_ICON: u16 = 3;
const RT_VERSION: u16 = 16;
const RT_GROUP_ICON: u16 = 14;
const RT_MANIFEST: u16 = 24;
const LANG_EN_US: u16 = 0x0409;

const PRODUCT: &str = "Lyrics UpFront";

const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity type="win32" name="LyricsUpFront" version="{VERSION}" processorArchitecture="amd64"/>
  <dependency>
    <dependentAssembly>
      <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0"
        processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/>
    </dependentAssembly>
  </dependency>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
    </windowsSettings>
  </application>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
    </application>
  </compatibility>
</assembly>
"#;

struct Resource {
    kind: u16,
    id: u16,
    data: Vec<u8>,
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/icon_art.rs");
    println!("cargo:rerun-if-changed=assets/app.ico");
    println!("cargo:rerun-if-changed=assets/app-paused.ico");
    println!("cargo:rerun-if-changed=assets/fonts/MonaSans-latin.woff2");
    println!("cargo:rerun-if-changed=Cargo.toml");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    // Icon groups 1 (the mark) and 2 (greyed, for the paused tray icon); the
    // RT_ICON images of both are numbered one after another.
    let mut resources: Vec<Resource> = Vec::new();
    let mut next_id = 1u16;
    for (group, ico) in [(icon_art::GROUP_ICON, icon_art::ICON), (icon_art::GROUP_ICON_PAUSED, icon_art::ICON_PAUSED)] {
        let images = icon_art::images(ico);
        resources.push(Resource { kind: RT_GROUP_ICON, id: group, data: icon_art::group_icon(&images, next_id) });
        for image in &images {
            resources.push(Resource { kind: RT_ICON, id: next_id, data: image.data.to_vec() });
            next_id += 1;
        }
    }
    let version = Version::from_cargo();
    let manifest = MANIFEST.replace("{VERSION}", &version.dotted4());
    resources.push(Resource { kind: RT_MANIFEST, id: 1, data: manifest.into_bytes() });
    resources.push(Resource { kind: RT_VERSION, id: 1, data: version_info(&version) });

    let object = coff_object(&resources);
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let ext = if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") { "obj" } else { "o" };
    let path = out.join(format!("lyrics_upfront_resources.{ext}"));
    std::fs::write(&path, object).expect("write resource object");
    println!("cargo:rustc-link-arg-bins={}", path.display());
}

/// The package version from Cargo.toml, as four numbers.
struct Version {
    text: String,
    parts: [u16; 4],
}

impl Version {
    fn from_cargo() -> Self {
        let text = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
        let mut parts = [0u16; 4];
        // "2.0.0" or "2.0.0-beta.1": take the leading numbers of each part.
        for (slot, piece) in parts.iter_mut().zip(text.split(['.', '-'])) {
            *slot = piece.parse().unwrap_or(0);
        }
        Version { text, parts }
    }

    fn dotted4(&self) -> String {
        let [a, b, c, d] = self.parts;
        format!("{a}.{b}.{c}.{d}")
    }

    fn ms_ls(&self) -> (u32, u32) {
        let [a, b, c, d] = self.parts.map(u32::from);
        ((a << 16) | b, (c << 16) | d)
    }
}

/// A VS_VERSIONINFO resource: fixed version numbers plus the US-English
/// strings Explorer and Task Manager read.
fn version_info(version: &Version) -> Vec<u8> {
    let (ms, ls) = version.ms_ls();
    let mut fixed = Vec::new();
    for x in [
        0xFEEF_04BD, // signature
        0x0001_0000, // structure version
        ms,          // file version
        ls,
        ms, // product version
        ls,
        0x3F,        // flags mask
        0,           // flags
        0x0004_0004, // VOS_NT_WINDOWS32
        1,           // VFT_APP
        0,           // subtype
        0,           // date
        0,
    ] {
        u32le(&mut fixed, x);
    }

    let exe = format!("{}.exe", std::env::var("CARGO_PKG_NAME").unwrap_or_default());
    let strings = [
        ("CompanyName", PRODUCT),
        ("FileDescription", PRODUCT),
        ("FileVersion", version.text.as_str()),
        ("InternalName", PRODUCT),
        ("LegalCopyright", "MIT License"),
        ("OriginalFilename", exe.as_str()),
        ("ProductName", PRODUCT),
        ("ProductVersion", version.text.as_str()),
    ];
    let string_blocks: Vec<Vec<u8>> = strings.iter().map(|(k, v)| version_block(k, BlockValue::Text(v), &[])).collect();
    let table = version_block("040904B0", BlockValue::None, &string_blocks);
    let string_file_info = version_block("StringFileInfo", BlockValue::None, &[table]);
    // Translation: language 0x0409 (US English), code page 1200 (Unicode).
    let translation = version_block("Translation", BlockValue::Binary(&[0x09, 0x04, 0xB0, 0x04]), &[]);
    let var_file_info = version_block("VarFileInfo", BlockValue::None, &[translation]);
    version_block("VS_VERSION_INFO", BlockValue::Binary(&fixed), &[string_file_info, var_file_info])
}

enum BlockValue<'a> {
    None,
    Binary(&'a [u8]),
    Text(&'a str),
}

/// One version-resource block: length, value length, type, key, value and
/// children, each starting on a 4-byte boundary.
fn version_block(key: &str, value: BlockValue, children: &[Vec<u8>]) -> Vec<u8> {
    fn utf16z(s: &str) -> Vec<u8> {
        s.encode_utf16().chain([0]).flat_map(|c| c.to_le_bytes()).collect()
    }
    fn align(v: &mut Vec<u8>) {
        while v.len() % 4 != 0 {
            v.push(0);
        }
    }
    let (value_bytes, value_len, kind) = match value {
        BlockValue::None => (Vec::new(), 0u16, 1u16),
        BlockValue::Binary(b) => (b.to_vec(), b.len() as u16, 0),
        // Text lengths count UTF-16 units, including the terminator.
        BlockValue::Text(s) => {
            let bytes = utf16z(s);
            let units = (bytes.len() / 2) as u16;
            (bytes, units, 1)
        }
    };
    let mut v = Vec::new();
    u16le(&mut v, 0); // length, patched below
    u16le(&mut v, value_len);
    u16le(&mut v, kind);
    v.extend_from_slice(&utf16z(key));
    align(&mut v);
    v.extend_from_slice(&value_bytes);
    for child in children {
        align(&mut v);
        v.extend_from_slice(child);
    }
    let len = v.len() as u16;
    v[0..2].copy_from_slice(&len.to_le_bytes());
    v
}

fn u16le(v: &mut Vec<u8>, x: u16) {
    v.extend_from_slice(&x.to_le_bytes());
}
fn u32le(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}

/// IMAGE_RESOURCE_DIRECTORY with `n` id entries.
fn directory_header(v: &mut Vec<u8>, n: u16) {
    u32le(v, 0); // characteristics
    u32le(v, 0); // time/date stamp
    u16le(v, 0); // major version
    u16le(v, 0); // minor version
    u16le(v, 0); // named entries
    u16le(v, n); // id entries
}

/// Build the `.rsrc` section (type -> id -> language -> data) and wrap it in
/// a one-section COFF object. Data entries hold RVAs, so each gets an
/// IMAGE_REL_AMD64_ADDR32NB relocation against the section symbol.
fn coff_object(resources: &[Resource]) -> Vec<u8> {
    let mut types: Vec<u16> = resources.iter().map(|r| r.kind).collect();
    types.sort_unstable();
    types.dedup();
    let ids_of = |t: u16| {
        let mut v: Vec<&Resource> = resources.iter().filter(|r| r.kind == t).collect();
        v.sort_by_key(|r| r.id);
        v
    };

    // Sizes of each region, to compute offsets before writing.
    let root_size = 16 + 8 * types.len();
    let type_dirs_size: usize = types.iter().map(|&t| 16 + 8 * ids_of(t).len()).sum();
    let lang_dirs_size = resources.len() * (16 + 8);
    let data_entries_size = resources.len() * 16;
    let data_start = root_size + type_dirs_size + lang_dirs_size + data_entries_size;

    // Assign offsets.
    let mut type_dir_offset = Vec::new();
    let mut off = root_size;
    for &t in &types {
        type_dir_offset.push(off);
        off += 16 + 8 * ids_of(t).len();
    }
    // Order resources by (type, id) for lang dirs, data entries and data.
    let mut ordered: Vec<&Resource> = Vec::new();
    for &t in &types {
        ordered.extend(ids_of(t));
    }
    let lang_dir_offset = |i: usize| root_size + type_dirs_size + i * 24;
    let data_entry_offset = |i: usize| root_size + type_dirs_size + lang_dirs_size + i * 16;
    let mut data_offset = Vec::new();
    let mut d = data_start;
    for r in &ordered {
        d = (d + 7) & !7;
        data_offset.push(d);
        d += r.data.len();
    }
    let section_size = (d + 3) & !3;

    let mut sec = Vec::with_capacity(section_size);
    // Root: one entry per type, pointing at its id directory.
    directory_header(&mut sec, types.len() as u16);
    for (i, &t) in types.iter().enumerate() {
        u32le(&mut sec, t as u32);
        u32le(&mut sec, 0x8000_0000 | type_dir_offset[i] as u32);
    }
    // Per type: one entry per id, pointing at its language directory.
    let mut index = 0;
    for &t in &types {
        let ids = ids_of(t);
        directory_header(&mut sec, ids.len() as u16);
        for r in ids {
            u32le(&mut sec, r.id as u32);
            u32le(&mut sec, 0x8000_0000 | lang_dir_offset(index) as u32);
            index += 1;
        }
    }
    // Per resource: one language entry, pointing at its data entry.
    for i in 0..ordered.len() {
        directory_header(&mut sec, 1);
        u32le(&mut sec, LANG_EN_US as u32);
        u32le(&mut sec, data_entry_offset(i) as u32);
    }
    // Data entries; OffsetToData is relocated to an RVA by the linker.
    let mut relocations = Vec::new();
    for (i, r) in ordered.iter().enumerate() {
        relocations.push(sec.len() as u32);
        u32le(&mut sec, data_offset[i] as u32);
        u32le(&mut sec, r.data.len() as u32);
        u32le(&mut sec, 0); // code page
        u32le(&mut sec, 0); // reserved
    }
    assert_eq!(sec.len(), data_start);
    for (i, r) in ordered.iter().enumerate() {
        sec.resize(data_offset[i], 0);
        sec.extend_from_slice(&r.data);
    }
    sec.resize(section_size, 0);

    // COFF object: file header, one section header, raw data, relocations,
    // a one-entry symbol table (the section symbol) and an empty string table.
    let header_size = 20 + 40;
    let reloc_ptr = header_size + sec.len();
    let symtab_ptr = reloc_ptr + relocations.len() * 10;
    let mut obj = Vec::new();
    u16le(&mut obj, 0x8664); // AMD64
    u16le(&mut obj, 1); // sections
    u32le(&mut obj, 0); // time/date stamp
    u32le(&mut obj, symtab_ptr as u32);
    u32le(&mut obj, 1); // symbols
    u16le(&mut obj, 0); // optional header size
    u16le(&mut obj, 0); // characteristics

    obj.extend_from_slice(b".rsrc\0\0\0");
    u32le(&mut obj, 0); // virtual size
    u32le(&mut obj, 0); // virtual address
    u32le(&mut obj, sec.len() as u32);
    u32le(&mut obj, header_size as u32);
    u32le(&mut obj, reloc_ptr as u32);
    u32le(&mut obj, 0); // line numbers
    u16le(&mut obj, relocations.len() as u16);
    u16le(&mut obj, 0);
    // CNT_INITIALIZED_DATA | ALIGN_4BYTES | MEM_READ
    u32le(&mut obj, 0x0000_0040 | 0x0030_0000 | 0x4000_0000);

    obj.extend_from_slice(&sec);
    for at in relocations {
        u32le(&mut obj, at);
        u32le(&mut obj, 0); // symbol index: .rsrc
        u16le(&mut obj, 3); // IMAGE_REL_AMD64_ADDR32NB
    }
    obj.extend_from_slice(b".rsrc\0\0\0");
    u32le(&mut obj, 0); // value
    u16le(&mut obj, 1); // section number
    u16le(&mut obj, 0); // type
    obj.push(3); // IMAGE_SYM_CLASS_STATIC
    obj.push(0); // aux symbols
    u32le(&mut obj, 4); // string table size
    obj
}
