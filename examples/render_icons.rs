//! Regenerates the raster logo files from `ui/icons/logo.svg`, the single source:
//!
//!     cargo run --example render_icons
//!
//! Writes `assets/gitust-512.png` (README) and `assets/gitust.ico` (16–256 px,
//! embedded in gitust.exe by build.rs). Sizes up to 24 px use the simplified
//! `assets/logo-small.svg`. The app itself draws the SVG directly.

use resvg::{tiny_skia, usvg};
use std::path::Path;

fn render(tree: &usvg::Tree, size: u32) -> Vec<u8> {
    let mut pixmap = tiny_skia::Pixmap::new(size, size).expect("non-zero size");
    let scale = size as f32 / tree.size().width();
    resvg::render(tree, tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    pixmap.encode_png().expect("PNG encoding")
}

/// An .ico whose entries are PNGs (supported by Windows Vista and later).
fn ico(images: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&[0, 0, 1, 0]);
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len() as u32;
    for (size, png) in images {
        let dim = if *size >= 256 { 0 } else { *size as u8 };
        out.extend_from_slice(&[dim, dim, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes()); // planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        out.extend_from_slice(&(png.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += png.len() as u32;
    }
    for (_, png) in images {
        out.extend_from_slice(png);
    }
    out
}

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let load = |rel: &str| {
        let svg = std::fs::read(root.join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"));
        usvg::Tree::from_data(&svg, &usvg::Options::default()).unwrap_or_else(|e| panic!("parse {rel}: {e}"))
    };
    let tree = load("ui/icons/logo.svg");
    let small = load("assets/logo-small.svg");

    std::fs::write(root.join("assets/gitust-512.png"), render(&tree, 512)).unwrap();
    let sizes = [16, 20, 24, 32, 40, 48, 64, 128, 256];
    let images: Vec<(u32, Vec<u8>)> =
        sizes.iter().map(|&s| (s, render(if s <= 24 { &small } else { &tree }, s))).collect();
    std::fs::write(root.join("assets/gitust.ico"), ico(&images)).unwrap();
    if let Some(dir) = std::env::args().nth(1) {
        // Optional preview folder: one PNG per size.
        for (s, png) in &images {
            std::fs::write(Path::new(&dir).join(format!("logo-{s}.png")), png).unwrap();
        }
    }
    println!("wrote assets/gitust-512.png and assets/gitust.ico ({} sizes)", sizes.len());
}
