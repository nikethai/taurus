//! `cargo xtask icons`: the app icon (drawn in code by the desktop crate)
//! as PNG, ICO and ICNS files for the installers.

use std::fs::{self, File};
use std::io::BufWriter;
use std::path::Path;

use icns::{IconFamily, IconType, Image, PixelFormat};
use ico::{IconDir, IconDirEntry, IconImage, ResourceType};

/// PNG sizes written as `<size>.png`.
const PNG_SIZES: [u32; 6] = [32, 64, 128, 256, 512, 1024];
/// Sizes inside `hyades.ico`.
const ICO_SIZES: [u32; 6] = [16, 24, 32, 48, 64, 256];
/// Pixel size and `.icns` type (ic04, ic11, ic12, ic07, ic08, ic09, ic10).
const ICNS_TYPES: [(u32, IconType); 7] = [
    (16, IconType::RGBA32_16x16_ARGB),
    (32, IconType::RGBA32_16x16_2x),
    (64, IconType::RGBA32_32x32_2x),
    (128, IconType::RGBA32_128x128),
    (256, IconType::RGBA32_256x256),
    (512, IconType::RGBA32_512x512),
    (1024, IconType::RGBA32_512x512_2x),
];

pub fn run(_args: &[String]) -> Result<(), String> {
    let dir = crate::repo_root().join("packaging/icons");
    write_all(&dir)?;
    println!("wrote {}", dir.display());
    Ok(())
}

pub fn write_all(dir: &Path) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for size in PNG_SIZES {
        write_png(&dir.join(format!("{size}.png")), size)?;
    }
    write_ico(&dir.join("hyades.ico"))?;
    write_icns(&dir.join("hyades.icns"))
}

fn io_err(path: &Path, e: impl std::fmt::Display) -> String {
    format!("{}: {e}", path.display())
}

fn write_png(path: &Path, size: u32) -> Result<(), String> {
    let file = File::create(path).map_err(|e| io_err(path, e))?;
    let mut encoder = png::Encoder::new(BufWriter::new(file), size, size);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|e| io_err(path, e))?;
    writer
        .write_image_data(&taurus_icon::app_icon_rgba(size))
        .map_err(|e| io_err(path, e))
}

fn write_ico(path: &Path) -> Result<(), String> {
    let mut dir = IconDir::new(ResourceType::Icon);
    for size in ICO_SIZES {
        let image = IconImage::from_rgba_data(size, size, taurus_icon::app_icon_rgba(size));
        let entry = IconDirEntry::encode(&image).map_err(|e| io_err(path, e))?;
        dir.add_entry(entry);
    }
    let file = File::create(path).map_err(|e| io_err(path, e))?;
    dir.write(BufWriter::new(file)).map_err(|e| io_err(path, e))
}

fn write_icns(path: &Path) -> Result<(), String> {
    let mut family = IconFamily::new();
    for (size, kind) in ICNS_TYPES {
        let image = Image::from_data(
            PixelFormat::RGBA,
            size,
            size,
            taurus_icon::app_icon_rgba(size),
        )
        .map_err(|e| io_err(path, e))?;
        family
            .add_icon_with_type(&image, kind)
            .map_err(|e| io_err(path, e))?;
    }
    let file = File::create(path).map_err(|e| io_err(path, e))?;
    family
        .write(BufWriter::new(file))
        .map_err(|e| io_err(path, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icons_have_expected_sizes() {
        let tmp = tempfile::tempdir().unwrap();
        write_all(tmp.path()).unwrap();

        for size in PNG_SIZES {
            let decoder = png::Decoder::new(std::io::BufReader::new(
                File::open(tmp.path().join(format!("{size}.png"))).unwrap(),
            ));
            let reader = decoder.read_info().unwrap();
            assert_eq!((reader.info().width, reader.info().height), (size, size));
        }

        let dir = IconDir::read(File::open(tmp.path().join("hyades.ico")).unwrap()).unwrap();
        let mut sizes: Vec<u32> = dir.entries().iter().map(|e| e.width()).collect();
        sizes.sort_unstable();
        assert_eq!(sizes, [16, 24, 32, 48, 64, 256]);

        let family = IconFamily::read(File::open(tmp.path().join("hyades.icns")).unwrap()).unwrap();
        let mut sizes: Vec<u32> = family
            .available_icons()
            .iter()
            .map(|t| t.pixel_width())
            .collect();
        sizes.sort_unstable();
        assert_eq!(sizes, [16, 32, 64, 128, 256, 512, 1024]);
    }
}
