//! Windows only: embeds the app icon and version information in `newskite.exe`, so
//! Explorer, the taskbar and the Start menu show the Newskite icon and the file's
//! properties name the product. The `.ico` is assembled here from the hicolor PNG renders
//! (an icon file may hold PNG images as they are), so it always matches the Linux icons.

use std::path::{Path, PathBuf};

/// Sizes taken from `packaging/icons/hicolor` (256 is the largest an `.ico` holds).
const ICON_SIZES: [u32; 7] = [16, 24, 32, 48, 64, 128, 256];

fn main() {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let hicolor = manifest.join("../../packaging/icons/hicolor");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", hicolor.display());
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let ico = out.join("newskite.ico");
    std::fs::write(&ico, ico_from_pngs(&hicolor)).expect("write newskite.ico");

    let version = std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION");
    let numbers: Vec<u16> = version
        .split(['.', '-', '+'])
        .take(3)
        .map(|n| n.parse().unwrap_or(0))
        .collect();
    let (major, minor, patch) = (numbers[0], numbers.get(1).copied().unwrap_or(0), numbers.get(2).copied().unwrap_or(0));
    let rc = out.join("newskite.rc");
    std::fs::write(&rc, resource_script(&ico, &version, [major, minor, patch])).expect("write newskite.rc");
    embed_resource::compile(&rc, embed_resource::NONE)
        .manifest_required()
        .expect("embed the icon and version resources");
}

/// An `.ico` file holding each PNG unchanged.
fn ico_from_pngs(hicolor: &Path) -> Vec<u8> {
    let images: Vec<(u32, Vec<u8>)> = ICON_SIZES
        .iter()
        .map(|&n| {
            let p = hicolor.join(format!("{n}x{n}/apps/net.innerpulse.Newskite.png"));
            (n, std::fs::read(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display())))
        })
        .collect();
    let mut ico = Vec::new();
    ico.extend_from_slice(&[0, 0, 1, 0]); // reserved, type 1 = icon
    ico.extend_from_slice(&(images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len() as u32;
    for (n, png) in &images {
        let side = if *n >= 256 { 0 } else { *n as u8 }; // 0 means 256
        ico.extend_from_slice(&[side, side, 0, 0]); // width, height, no palette, reserved
        ico.extend_from_slice(&1u16.to_le_bytes()); // color planes
        ico.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        ico.extend_from_slice(&(png.len() as u32).to_le_bytes());
        ico.extend_from_slice(&offset.to_le_bytes());
        offset += png.len() as u32;
    }
    for (_, png) in &images {
        ico.extend_from_slice(png);
    }
    ico
}

/// The resource script: the icon (first, so it is the file's icon) and VERSIONINFO.
fn resource_script(ico: &Path, version: &str, [major, minor, patch]: [u16; 3]) -> String {
    // rc reads backslashes in strings as escapes; forward slashes work as separators.
    let ico = ico.display().to_string().replace('\\', "/");
    format!(
        r#"#pragma code_page(65001)
1 ICON "{ico}"

1 VERSIONINFO
FILEVERSION {major},{minor},{patch},0
PRODUCTVERSION {major},{minor},{patch},0
FILEOS 0x40004
FILETYPE 0x1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "innerpulse"
      VALUE "FileDescription", "Newskite"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "newskite"
      VALUE "OriginalFilename", "newskite.exe"
      VALUE "ProductName", "Newskite"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
    )
}
