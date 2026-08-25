use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use image::codecs::jpeg::JpegEncoder;
use image::{ColorType, GenericImageView};

const MAX_EDGE: u32 = 360;
const JPEG_QUALITY: u8 = 70;
const MAX_SOURCE_BYTES: u64 = 40 * 1024 * 1024;

fn is_raster_image(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .as_deref(),
        Some("jpg" | "jpeg" | "png" | "gif" | "bmp")
    )
}

fn fnv1a64(data: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in data {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

pub fn cache_path(data_root: &Path, source: &Path, meta: &std::fs::Metadata) -> PathBuf {
    let mtime = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let key = format!("{}:{}:{}", source.to_string_lossy(), meta.len(), mtime);
    data_root
        .join("thumbs")
        .join(format!("{:016x}.jpg", fnv1a64(key.as_bytes())))
}

pub fn generate_jpeg_thumb(source: &Path, dest: &Path) -> Result<(), String> {
    let img = image::open(source).map_err(|err| err.to_string())?;
    let (width, height) = img.dimensions();
    let thumb = if width <= MAX_EDGE && height <= MAX_EDGE {
        img
    } else {
        img.thumbnail(MAX_EDGE, MAX_EDGE)
    };
    let rgb = thumb.to_rgb8();
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    let tmp = dest.with_extension("jpg.part");
    let mut file = std::fs::File::create(&tmp).map_err(|err| err.to_string())?;
    let mut encoder = JpegEncoder::new_with_quality(&mut file, JPEG_QUALITY);
    encoder
        .encode(rgb.as_raw(), rgb.width(), rgb.height(), ColorType::Rgb8)
        .map_err(|err| err.to_string())?;
    drop(file);
    std::fs::rename(&tmp, dest).map_err(|err| err.to_string())?;
    Ok(())
}

/// 已有缓存直接返回；非图片或过大则 `None`（调用方改走原图）。
pub async fn ensure_jpeg_thumb(data_root: &Path, source: &Path) -> Result<Option<PathBuf>, String> {
    if !is_raster_image(source) {
        return Ok(None);
    }
    let meta = std::fs::metadata(source).map_err(|err| err.to_string())?;
    if !meta.is_file() || meta.len() == 0 || meta.len() > MAX_SOURCE_BYTES {
        return Ok(None);
    }
    let dest = cache_path(data_root, source, &meta);
    if dest.is_file() {
        return Ok(Some(dest));
    }
    let source = source.to_path_buf();
    tokio::task::spawn_blocking(move || {
        generate_jpeg_thumb(&source, &dest)?;
        Ok(Some(dest))
    })
    .await
    .map_err(|err| err.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tgd-thumb-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn skips_non_image_extension() {
        let dir = temp_dir("skip");
        let src = dir.join("clip.mp4");
        std::fs::write(&src, b"not-an-image").unwrap();
        assert!(!is_raster_image(&src));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn writes_shrunk_jpeg() {
        let dir = temp_dir("jpeg");
        let src = dir.join("big.png");
        RgbImage::from_pixel(800, 600, Rgb([200, 40, 40]))
            .save(&src)
            .unwrap();
        let dest = dir.join("thumbs").join("a.jpg");
        generate_jpeg_thumb(&src, &dest).unwrap();
        let thumb = image::open(&dest).unwrap();
        assert!(thumb.width() <= MAX_EDGE);
        assert!(thumb.height() <= MAX_EDGE);
        assert!(dest.metadata().unwrap().len() < src.metadata().unwrap().len());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_path_changes_with_mtime_or_size() {
        let dir = temp_dir("cache");
        let src = dir.join("a.jpg");
        std::fs::write(&src, b"abc").unwrap();
        let meta = std::fs::metadata(&src).unwrap();
        let first = cache_path(&dir, &src, &meta);
        std::fs::write(&src, b"abcdef").unwrap();
        let meta = std::fs::metadata(&src).unwrap();
        let second = cache_path(&dir, &src, &meta);
        assert_ne!(first, second);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
