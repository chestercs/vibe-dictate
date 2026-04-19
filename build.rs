use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "windows" {
        return;
    }

    let icon_path = Path::new("assets/icon.ico");
    if !icon_path.exists() {
        if let Err(e) = generate_icon(icon_path) {
            println!("cargo:warning=icon generation failed: {e}");
            return;
        }
    }

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let rc_path = out_dir.join("vibe_dictate.rc");
    let icon_abs = fs::canonicalize(icon_path).unwrap_or_else(|_| icon_path.to_path_buf());
    let icon_ref = icon_abs.to_string_lossy().replace('\\', "\\\\");
    fs::write(&rc_path, format!("1 ICON \"{}\"\n", icon_ref)).unwrap();

    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=build.rs");

    let _ = embed_resource::compile(&rc_path, embed_resource::NONE);
}

fn generate_icon(out: &Path) -> std::io::Result<()> {
    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent)?;
    }

    // Same motif as the tray idle icon: flat blue square (30, 120, 220) with a
    // centered white dot. Dot radius = size / 8 keeps it proportional across
    // the 16/32/48/256 px ICO entries Windows may ask for.
    let sizes: &[u32] = &[16, 32, 48, 256];
    let images: Vec<Vec<u8>> = sizes.iter().map(|&s| render_bmp(s)).collect();

    let mut ico = Vec::new();
    ico.extend_from_slice(&0u16.to_le_bytes()); // reserved
    ico.extend_from_slice(&1u16.to_le_bytes()); // type = icon
    ico.extend_from_slice(&(images.len() as u16).to_le_bytes());

    let dir_size = 6 + 16 * images.len();
    let mut offset = dir_size as u32;
    for (i, data) in images.iter().enumerate() {
        let size = sizes[i];
        let dim: u8 = if size >= 256 { 0 } else { size as u8 };
        ico.push(dim); // width
        ico.push(dim); // height
        ico.push(0); // palette size
        ico.push(0); // reserved
        ico.extend_from_slice(&1u16.to_le_bytes()); // planes
        ico.extend_from_slice(&32u16.to_le_bytes()); // bpp
        ico.extend_from_slice(&(data.len() as u32).to_le_bytes());
        ico.extend_from_slice(&offset.to_le_bytes());
        offset += data.len() as u32;
    }
    for data in &images {
        ico.extend_from_slice(data);
    }

    fs::write(out, ico)
}

fn render_bmp(size: u32) -> Vec<u8> {
    // BITMAPINFOHEADER with biHeight = size*2 (Windows ICO convention: XOR
    // pixel plane stacked with a trailing 1-bpp AND mask — we leave the mask
    // zeroed because the alpha channel already carries transparency info).
    let mut buf = Vec::new();
    buf.extend_from_slice(&40u32.to_le_bytes()); // biSize
    buf.extend_from_slice(&(size as i32).to_le_bytes()); // biWidth
    buf.extend_from_slice(&((size * 2) as i32).to_le_bytes()); // biHeight
    buf.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    buf.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    buf.extend_from_slice(&0u32.to_le_bytes()); // biCompression (BI_RGB)
    buf.extend_from_slice(&0u32.to_le_bytes()); // biSizeImage
    buf.extend_from_slice(&0i32.to_le_bytes()); // biXPelsPerMeter
    buf.extend_from_slice(&0i32.to_le_bytes()); // biYPelsPerMeter
    buf.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
    buf.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant

    let (r, g, b) = (30u8, 120u8, 220u8);
    let dot_r = (size as i32 / 8).max(2);
    let dot_r2 = dot_r * dot_r;
    let cx = (size as i32) / 2;
    let cy = (size as i32) / 2;

    // BMP pixel rows are bottom-up, BGRA byte order.
    for y in (0..size).rev() {
        for x in 0..size {
            let dx = x as i32 - cx;
            let dy = y as i32 - cy;
            let (pr, pg, pb) = if dx * dx + dy * dy < dot_r2 {
                (255u8, 255u8, 255u8)
            } else {
                (r, g, b)
            };
            buf.push(pb);
            buf.push(pg);
            buf.push(pr);
            buf.push(255); // alpha
        }
    }

    let mask_row_bytes = (((size + 31) / 32) * 4) as usize;
    let mask_size = mask_row_bytes * size as usize;
    buf.extend(std::iter::repeat(0u8).take(mask_size));

    buf
}
