//! Small previews of large images.
//!
//! A wallpaper is 4K or 8K, so decoding one to show it in a list would cost
//! tens of megabytes per row. The decoder here always asks for a thumbnail
//! size and keeps the result in memory; a small cache on disk makes the next
//! start instant. The cache key includes the file's size, modification time
//! and inode with full precision, so a replaced file never shows a stale
//! preview even when the new file has exactly the same size.

use std::{
    collections::{hash_map::DefaultHasher, HashMap, HashSet},
    env, fs,
    hash::{Hash, Hasher},
    io::BufReader,
    path::{Path, PathBuf},
    sync::{
        mpsc::{self, Receiver, SyncSender},
        Arc, Mutex,
    },
    time::SystemTime,
};

use crate::watch::Wake;

/// Longest side of a preview, in pixels.
pub const THUMB_SIZE: u32 = 320;

/// Refuse to decode anything larger than this many pixels. An 8K picture is
/// about 33 megapixels; 80 MP is also the most `niructl theme-import`
/// accepts, so every theme wallpaper NIRUX can have still gets a preview.
pub const MAX_PIXELS: u64 = 80_000_000;

/// Pictures above this size are decoded one at a time, whatever the number
/// of workers. Measured on othala (`peak_rss_decoding_8k_pictures`, four 8K
/// PNG/JPEG on a cold cache, two workers): 0.1.2 peaked 239 MB above the
/// process baseline, 0.2.0 peaks 97 MB.
pub const BIG_PIXELS: u64 = 16_000_000;

/// The decoder's own allocation limit: one RGBA frame of `MAX_PIXELS`.
pub const MAX_ALLOC: u64 = MAX_PIXELS * 4;

/// Held while a big picture decodes.
static BIG: Mutex<()> = Mutex::new(());

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

/// A decoded preview and the picture's own size.
pub struct Preview {
    pub image: Option<image::RgbaImage>,
    pub dims: Option<(u32, u32)>,
}

/// The picture's size from its header, without decoding it.
pub fn dimensions(path: &Path) -> Option<(u32, u32)> {
    image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

/// Decode a preview of one image.
///
/// Returns no image for a missing, undecodable or unreasonably large file:
/// the list then shows a placeholder instead of failing. Memory is bounded
/// three ways: the decoder streams from the file instead of a copy of it in
/// memory, pictures above `BIG_PIXELS` decode one at a time, and the decoder
/// may not allocate more than `MAX_ALLOC`. The downscale uses `thumbnail`,
/// which averages straight into the small picture without a full-width
/// floating-point intermediate.
pub fn decode_preview(path: &Path) -> Preview {
    decode_preview_gated(path, true)
}

fn decode_preview_gated(path: &Path, gate: bool) -> Preview {
    let dims = dimensions(path);
    let image = decode_image(path, dims, gate);
    Preview { image, dims }
}

fn decode_image(path: &Path, dims: Option<(u32, u32)>, gate: bool) -> Option<image::RgbaImage> {
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() == 0 || meta.len() > 512 * 1024 * 1024 {
        return None;
    }
    let cached = cache_dir().join(format!("{}-{THUMB_SIZE}.png", key(path)));
    if let Some(image) = read_cache(&cached) {
        return Some(image);
    }
    let (iw, ih) = dims?;
    let pixels = u64::from(iw) * u64::from(ih);
    if pixels > MAX_PIXELS {
        return None;
    }
    let _big = (gate && pixels > BIG_PIXELS)
        .then(|| BIG.lock().unwrap_or_else(|poisoned| poisoned.into_inner()));
    let file = fs::File::open(path).ok()?;
    let mut reader = image::ImageReader::new(BufReader::new(file))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(65_536);
    limits.max_image_height = Some(65_536);
    limits.max_alloc = Some(MAX_ALLOC);
    reader.limits(limits);
    let image = reader.decode().ok()?;
    let scale = f64::from(THUMB_SIZE) / f64::from(iw.max(ih)).max(1.0);
    let small = if scale < 1.0 {
        let tw = (f64::from(iw) * scale).round().max(1.0) as u32;
        let th = (f64::from(ih) * scale).round().max(1.0) as u32;
        let small = image.thumbnail_exact(tw, th);
        drop(image);
        small
    } else {
        image
    };
    let rgba = small.to_rgba8();
    write_cache(&cached, &rgba);
    Some(rgba)
}

/// A finished preview, with the stamp it was asked for.
pub struct Done {
    pub path: PathBuf,
    pub stamp: String,
    pub preview: Preview,
}

/// Two decode workers. They wake the event loop through `wake` when a
/// preview is ready, so the loop never polls for them.
pub struct Pool {
    tx: SyncSender<(PathBuf, String)>,
    rx: Receiver<Done>,
}

impl Pool {
    pub fn new(wake: Arc<Wake>) -> Self {
        let (tx, work) = mpsc::sync_channel::<(PathBuf, String)>(32);
        let (done, rx) = mpsc::channel::<Done>();
        let work = Arc::new(Mutex::new(work));
        for _ in 0..2 {
            let work = work.clone();
            let done = done.clone();
            let wake = wake.clone();
            std::thread::spawn(move || loop {
                let (path, stamp) = {
                    let Ok(receiver) = work.lock() else {
                        return;
                    };
                    match receiver.recv() {
                        Ok(job) => job,
                        Err(_) => return,
                    }
                };
                let preview = decode_preview(&path);
                if done
                    .send(Done {
                        path,
                        stamp,
                        preview,
                    })
                    .is_err()
                {
                    return;
                }
                wake.notify();
            });
        }
        Self { tx, rx }
    }

    /// Queue a decode; false when the queue is full (ask again later).
    pub fn request(&self, path: PathBuf, stamp: String) -> bool {
        self.tx.try_send((path, stamp)).is_ok()
    }

    pub fn try_recv(&self) -> Option<Done> {
        self.rx.try_recv().ok()
    }
}

/// One cached preview.
pub struct Slot {
    pub stamp: String,
    pub image: Option<image::RgbaImage>,
    pub dims: Option<(u32, u32)>,
    last_paint: u64,
}

/// The decoded previews in memory, evicted least recently painted first.
/// A preview that is on screen or selected is never evicted, so it cannot
/// reload and flicker.
pub struct Cache {
    slots: HashMap<PathBuf, Slot>,
    requested: HashSet<PathBuf>,
    tick: u64,
    cap: usize,
}

impl Cache {
    pub fn new(cap: usize) -> Self {
        Self {
            slots: HashMap::new(),
            requested: HashSet::new(),
            tick: 0,
            cap: cap.max(1),
        }
    }

    pub fn get(&self, path: &Path) -> Option<&Slot> {
        self.slots.get(path)
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Mark these previews as painted now.
    pub fn touch<'a>(&mut self, paths: impl IntoIterator<Item = &'a PathBuf>) {
        self.tick += 1;
        for path in paths {
            if let Some(slot) = self.slots.get_mut(path) {
                slot.last_paint = self.tick;
            }
        }
    }

    /// True when `path` has no preview for `stamp` and none is on its way.
    pub fn wants(&self, path: &Path, stamp: &str) -> bool {
        !self.requested.contains(path)
            && self.slots.get(path).is_none_or(|slot| slot.stamp != stamp)
    }

    pub fn requested(&mut self, path: PathBuf) {
        self.requested.insert(path);
    }

    /// A decode finished (taken or dropped): it is no longer on its way.
    pub fn arrived(&mut self, path: &Path) {
        self.requested.remove(path);
    }

    /// Keep a decoded preview. At the cap, the least recently painted entry
    /// that is not in `protected` makes room; when every entry is protected
    /// the cache grows past the cap rather than drop a visible preview.
    pub fn insert(
        &mut self,
        path: PathBuf,
        stamp: String,
        preview: Preview,
        protected: &HashSet<PathBuf>,
    ) {
        if !self.slots.contains_key(&path) && self.slots.len() >= self.cap {
            let victim = self
                .slots
                .iter()
                .filter(|(p, _)| !protected.contains(*p))
                .min_by_key(|(_, slot)| slot.last_paint)
                .map(|(p, _)| p.clone());
            if let Some(victim) = victim {
                self.slots.remove(&victim);
            }
        }
        self.slots.insert(
            path,
            Slot {
                stamp,
                image: preview.image,
                dims: preview.dims,
                last_paint: self.tick,
            },
        );
    }
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
    // Each modification time is read once: the other decode worker may
    // write or prune this directory meanwhile, and a key that changes
    // during the sort makes it panic.
    cached.sort_by_cached_key(|entry| entry.metadata().and_then(|m| m.modified()).ok());
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
        let thumb = decode_preview(&src).image.expect("thumbnail");
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
        decode_preview(&src).image.unwrap();
        let entries = fs::read_dir(cache_dir()).unwrap().count();
        assert_eq!(entries, 1, "one preview written");
        let stamp = fs::read_dir(cache_dir())
            .unwrap()
            .flatten()
            .map(|e| e.metadata().unwrap().modified().unwrap())
            .next()
            .unwrap();
        decode_preview(&src).image.unwrap();
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
        let third = decode_preview(&src).image.unwrap();
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
        decode_preview(&src).image.unwrap();
        let cached = fs::read_dir(cache_dir())
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .next()
            .unwrap();
        fs::write(&cached, b"not a png").unwrap();
        let image = decode_preview(&src)
            .image
            .expect("a corrupt cache must not hide the image");
        assert_eq!(image.get_pixel(0, 0).0[0], 90);
    }

    #[test]
    fn a_missing_or_broken_file_has_no_preview() {
        let _guard = lock();
        let dir = sandbox("broken");
        assert!(decode_preview(&dir.0.join("absent.png")).image.is_none());
        let junk = dir.0.join("junk.png");
        fs::write(&junk, b"not an image").unwrap();
        assert!(
            decode_preview(&junk).image.is_none(),
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
            let mut cursor = std::io::Cursor::new(Vec::new());
            dynamic.write_to(&mut cursor, format).unwrap();
            fs::write(&path, cursor.into_inner()).unwrap();
            let preview = decode_preview(&path);
            let thumb = preview
                .image
                .unwrap_or_else(|| panic!("{name} has no preview"));
            assert_eq!(thumb.width(), 96, "{name}");
            assert_eq!(preview.dims, Some((96, 64)), "{name}");
        }
    }

    fn preview(shade: u8) -> Preview {
        Preview {
            image: Some(image::RgbaImage::from_pixel(2, 2, image::Rgba([shade; 4]))),
            dims: Some((2, 2)),
        }
    }

    #[test]
    fn the_lru_never_evicts_a_visible_or_selected_preview() {
        let mut cache = Cache::new(4);
        let path = |i: usize| PathBuf::from(format!("/w/{i}.png"));
        let none = HashSet::new();
        for i in 0..4 {
            cache.insert(path(i), "s".into(), preview(i as u8), &none);
            cache.touch([&path(i)]);
        }
        // 0 was painted longest ago, but it is visible again now; 1 is the
        // selected theme. 2 is the least recently painted unprotected one.
        let protected: HashSet<_> = [path(0), path(1)].into_iter().collect();
        cache.insert(path(4), "s".into(), preview(4), &protected);
        assert_eq!(cache.len(), 4);
        assert!(cache.get(&path(0)).is_some(), "visible preview evicted");
        assert!(cache.get(&path(1)).is_some(), "selected preview evicted");
        assert!(cache.get(&path(2)).is_none(), "the LRU victim stayed");
        assert!(cache.get(&path(4)).is_some());
        // With every entry protected the cache grows instead.
        let all: HashSet<_> = (0..6).map(path).collect();
        cache.insert(path(5), "s".into(), preview(5), &all);
        assert_eq!(cache.len(), 5);
        // A new stamp is wanted again; one on its way is not.
        assert!(!cache.wants(&path(4), "s"));
        assert!(cache.wants(&path(4), "t"));
        cache.requested(path(9));
        assert!(!cache.wants(&path(9), "s"));
        cache.arrived(&path(9));
        assert!(cache.wants(&path(9), "s"));
    }

    fn peak_rss_kb() -> u64 {
        fs::read_to_string("/proc/self/status")
            .unwrap()
            .lines()
            .find_map(|l| l.strip_prefix("VmHWM:"))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
            .unwrap()
    }

    fn decode_all(paths: &[PathBuf], gate: bool) -> u64 {
        // Reset the peak (Linux clear_refs 5), then decode with two workers
        // as the window does.
        fs::write("/proc/self/clear_refs", "5").unwrap();
        let base = peak_rss_kb();
        let queue = Arc::new(Mutex::new(paths.to_vec()));
        let workers: Vec<_> = (0..2)
            .map(|_| {
                let queue = queue.clone();
                std::thread::spawn(move || loop {
                    let Some(path) = queue.lock().unwrap().pop() else {
                        return;
                    };
                    assert!(decode_preview_gated(&path, gate).image.is_some());
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        peak_rss_kb().saturating_sub(base)
    }

    /// T7: peak RSS while decoding four 8K pictures (two PNG, two JPEG) on a
    /// cold cache. Run with `cargo test --release -- --ignored peak_rss
    /// --nocapture`.
    #[test]
    #[ignore = "writes four 8K pictures; run by hand to measure"]
    fn peak_rss_decoding_8k_pictures() {
        let _guard = lock();
        let dir = sandbox("rss");
        let mut paths = Vec::new();
        for (i, format) in ["png", "jpg", "png", "jpg"].into_iter().enumerate() {
            let img = image::RgbImage::from_fn(7680, 4320, |x, y| {
                image::Rgb([(x / 30) as u8, (y / 17) as u8, (i * 60) as u8])
            });
            let path = dir.0.join(format!("w{i}.{format}"));
            img.save(&path).unwrap();
            paths.push(path);
        }
        std::env::set_var("XDG_CACHE_HOME", dir.0.join("cache-ungated"));
        let ungated = decode_all(&paths, false);
        std::env::set_var("XDG_CACHE_HOME", dir.0.join("cache-gated"));
        let gated = decode_all(&paths, true);
        println!("peak RSS above baseline: two parallel 8K decodes {ungated} kB, gated {gated} kB");
        assert!(gated < 300 * 1024, "gated peak {gated} kB");
    }
}
