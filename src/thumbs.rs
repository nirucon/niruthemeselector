//! Small previews of large images.
//!
//! A wallpaper is 4K or 8K, so decoding one to show it in a list would cost
//! tens of megabytes per row. The decoder here always asks for a thumbnail
//! size and keeps the result in memory; a small cache on disk makes the next
//! start instant. The cache key includes the file's size, modification time
//! and inode with full precision, so a replaced file never shows a stale
//! preview even when the new file has exactly the same size.

use std::{
    collections::hash_map::DefaultHasher,
    env, fs,
    hash::{Hash, Hasher},
    io::Cursor,
    path::{Path, PathBuf},
    time::SystemTime,
};

/// Longest side of a preview, in pixels.
pub const THUMB_SIZE: u32 = 320;

/// Refuse to decode anything larger than this many pixels. An 8K picture is
/// about 34 megapixels, so this leaves room for 16K while keeping a corrupt
/// or hostile header from asking for gigabytes.
pub const MAX_PIXELS: u64 = 200_000_000;

fn cache_dir() -> PathBuf {
    env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join(".cache")
        })
        .join("niruthemeselector")
        .join("thumbs-v2")
}

/// A cache key that changes when the file's identity or content changes.
///
/// Size and modification time (with full sub-second precision) catch nearly
/// every replacement. The inode and change time are deliberately not part of
/// the key: writing the same bytes twice gives them new values and would throw
/// away a perfectly good preview. As a last resort the first and last 64 KiB
/// are hashed, so two quick same-size replacements of a large file still get
/// distinct keys.
pub(crate) fn key(path: &Path) -> String {
    let meta = fs::metadata(path).ok();
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    if let Some(meta) = meta {
        meta.len().hash(&mut hasher);
        meta.modified()
            .ok()
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .hash(&mut hasher);
    }
    for bytes in edge_bytes(path) {
        bytes.hash(&mut hasher);
    }
    format!("{:016x}", hasher.finish())
}

/// The first and last 64 KiB of a file, read separately so a 100 MB picture
/// never costs a whole extra read just to name its preview.
fn edge_bytes(path: &Path) -> Vec<Vec<u8>> {
    const EDGE: u64 = 64 * 1024;
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = fs::File::open(path) else {
        return Vec::new();
    };
    let Ok(len) = file.metadata().map(|m| m.len()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut head = vec![0u8; EDGE.min(len) as usize];
    if file.read_exact(&mut head).is_ok() {
        out.push(head);
    }
    if len > EDGE * 2 {
        let mut tail = vec![0u8; EDGE as usize];
        if file.seek(SeekFrom::Start(len - EDGE)).is_ok() && file.read_exact(&mut tail).is_ok() {
            out.push(tail);
        }
    }
    out
}

/// Decode a preview of one image.
///
/// Returns `None` for a missing, undecodable or unreasonably large file: the
/// list then shows a placeholder instead of failing. The file is read in one
/// operation and decoded from memory, which avoids passing a file descriptor
/// around a decoder that keeps it open.
pub fn decode_preview(path: &Path) -> Option<image::RgbaImage> {
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() == 0 || meta.len() > 512 * 1024 * 1024 {
        return None;
    }
    let cached = cache_dir().join(format!("{}-{THUMB_SIZE}.png", key(path)));
    if let Some(image) = read_cache(&cached) {
        return Some(image);
    }
    let bytes = fs::read(path).ok()?;
    let reader = image::ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(65_536);
    limits.max_image_height = Some(65_536);
    limits.max_alloc = Some(MAX_PIXELS * 5);
    let mut reader = reader;
    reader.limits(limits);
    let format = reader.format()?;
    let decoder = reader.into_decoder().ok()?;
    // Ask the decoder for the target size: most formats read the header
    // first, so this is the point where a large picture costs a small decode.
    use image::ImageDecoder as _;
    let (iw, ih) = decoder.dimensions();
    if u64::from(iw) * u64::from(ih) > MAX_PIXELS {
        return None;
    }
    let scale = f64::from(THUMB_SIZE) / f64::from(iw.max(ih)).max(1.0);
    let (tw, th) = if scale < 1.0 {
        (
            (f64::from(iw) * scale).round().max(1.0) as u32,
            (f64::from(ih) * scale).round().max(1.0) as u32,
        )
    } else {
        (iw, ih)
    };
    let image = image::DynamicImage::from_decoder(decoder).ok()?;
    let small = if scale < 1.0 {
        image.resize_exact(tw, th, image::imageops::FilterType::Triangle)
    } else {
        image
    };
    let rgba = small.to_rgba8();
    let _ = format;
    write_cache(&cached, &rgba);
    Some(rgba)
}

fn read_cache(path: &Path) -> Option<image::RgbaImage> {
    let bytes = fs::read(path).ok()?;
    let image = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png).ok()?;
    // A truncated or corrupt cache file must never be shown as a preview.
    if image.width() == 0 || image.height() == 0 {
        return None;
    }
    Some(image.to_rgba8())
}

fn write_cache(path: &Path, image: &image::RgbaImage) {
    let Some(dir) = path.parent() else {
        return;
    };
    if fs::create_dir_all(dir).is_err() {
        return;
    }
    // Limit retained generations; replacing a large image must not grow cache forever.
    let mut cached: Vec<_> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().extension().and_then(|e| e.to_str()) == Some("png"))
        .collect();
    cached.sort_by_key(|entry| entry.metadata().and_then(|m| m.modified()).ok());
    let excess = cached.len().saturating_sub(127);
    for entry in cached.into_iter().take(excess) {
        let _ = fs::remove_file(entry.path());
    }
    // Write beside the target and rename, so a second instance never reads a
    // half-written preview.
    let temp = path.with_file_name(format!(
        ".{}.{}.tmp.png",
        std::process::id(),
        path.file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default()
    ));
    if image.save(&temp).is_err() {
        let _ = fs::remove_file(&temp);
        return;
    }
    let _ = fs::rename(&temp, path);
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Sandbox(PathBuf);

    impl Drop for Sandbox {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }

    /// The cache location is process-wide, so preview tests run one at a time.
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn sandbox(name: &str) -> Sandbox {
        // One cache root per test, so tests running in parallel never see
        // each other's entries.
        let dir = std::env::temp_dir().join(format!("nw-thumb-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // Every test has its own cache root, so a shared runtime directory
        // is never touched.
        std::env::set_var("XDG_CACHE_HOME", dir.join(".cache"));
        Sandbox(dir)
    }

    fn write_png(path: &Path, w: u32, h: u32, shade: u8) {
        let mut img = image::RgbaImage::new(w, h);
        for pixel in img.pixels_mut() {
            *pixel = image::Rgba([shade, shade, shade, 255]);
        }
        img.save(path).unwrap();
    }

    #[test]
    fn a_large_image_yields_a_small_preview() {
        let _guard = lock();
        let dir = sandbox("large");
        let src = dir.0.join("big.png");
        write_png(&src, 1920, 1080, 40);
        let thumb = decode_preview(&src).expect("thumbnail");
        assert!(
            thumb.width() <= THUMB_SIZE && thumb.height() <= THUMB_SIZE,
            "{thumb:?}"
        );
        // The aspect ratio is kept, so the preview is not squared off.
        assert!(thumb.width() == THUMB_SIZE);
        assert!(thumb.height() < THUMB_SIZE);
    }

    #[test]
    fn the_preview_cache_is_reused_until_the_image_changes() {
        let _guard = lock();
        let dir = sandbox("cached");
        let src = dir.0.join("wall.png");
        write_png(&src, 640, 360, 10);
        decode_preview(&src).unwrap();
        let entries = fs::read_dir(cache_dir()).unwrap().count();
        assert_eq!(entries, 1, "one preview written");
        let stamp = fs::read_dir(cache_dir())
            .unwrap()
            .flatten()
            .map(|e| e.metadata().unwrap().modified().unwrap())
            .next()
            .unwrap();
        decode_preview(&src).unwrap();
        assert_eq!(
            fs::read_dir(cache_dir())
                .unwrap()
                .flatten()
                .map(|e| e.metadata().unwrap().modified().unwrap())
                .next()
                .unwrap(),
            stamp,
            "the second call must reuse the cached file"
        );

        // A same-size replacement must produce a new preview and a new shade.
        write_png(&src, 640, 360, 200);
        let third = decode_preview(&src).unwrap();
        assert_eq!(
            third.get_pixel(0, 0).0[0],
            200,
            "a changed image kept its old preview"
        );
        assert_eq!(
            fs::read_dir(cache_dir()).unwrap().count(),
            2,
            "each version has its own entry"
        );
    }

    #[test]
    fn a_corrupt_cache_entry_is_regenerated() {
        let _guard = lock();
        let dir = sandbox("corrupt");
        let src = dir.0.join("wall.png");
        write_png(&src, 320, 200, 90);
        decode_preview(&src).unwrap();
        let cached = fs::read_dir(cache_dir())
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .next()
            .unwrap();
        fs::write(&cached, b"not a png").unwrap();
        let image = decode_preview(&src).expect("a corrupt cache must not hide the image");
        assert_eq!(image.get_pixel(0, 0).0[0], 90);
    }

    #[test]
    fn a_missing_or_broken_file_has_no_preview() {
        let _guard = lock();
        let dir = sandbox("broken");
        assert!(decode_preview(&dir.0.join("absent.png")).is_none());
        let junk = dir.0.join("junk.png");
        fs::write(&junk, b"not an image").unwrap();
        assert!(
            decode_preview(&junk).is_none(),
            "a broken file must not panic"
        );
    }

    #[test]
    fn jpeg_webp_and_bmp_are_previewed_like_png() {
        let _guard = lock();
        let dir = sandbox("formats");
        let rgba = image::RgbaImage::from_pixel(96, 64, image::Rgba([20, 90, 160, 255]));
        let dynamic = image::DynamicImage::ImageRgba8(rgba);
        for (name, format) in [
            ("a.jpg", image::ImageFormat::Jpeg),
            ("b.webp", image::ImageFormat::WebP),
            ("c.bmp", image::ImageFormat::Bmp),
            ("d.png", image::ImageFormat::Png),
        ] {
            let path = dir.0.join(name);
            let mut cursor = Cursor::new(Vec::new());
            dynamic.write_to(&mut cursor, format).unwrap();
            fs::write(&path, cursor.into_inner()).unwrap();
            let thumb = decode_preview(&path).unwrap_or_else(|| panic!("{name} has no preview"));
            assert_eq!(thumb.width(), 96, "{name}");
        }
    }
}
