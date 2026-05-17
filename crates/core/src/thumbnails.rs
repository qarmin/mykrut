//! Freedesktop Thumbnail Managing Standard implementation.
//!
//! Cache layout under `$XDG_CACHE_HOME/thumbnails/`:
//! ```text
//! normal/<md5(uri)>.png    (128 px)
//! large/<md5(uri)>.png     (256 px)
//! x-large/<md5(uri)>.png   (512 px)
//! xx-large/<md5(uri)>.png  (1024 px)
//! fail/<app-name>/<md5(uri)>.png   (negative cache)
//! ```
//!
//! The URI is `file://<absolute path>` per spec.
//!
//! We deliberately keep this synchronous + Send-friendly so callers can run it
//! on a rayon thread pool. Anything Slint-aware lives in the app crate.

use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, anyhow};
use image::ImageDecoder;
use md5::{Digest, Md5};
use tracing::{debug, warn};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThumbSize {
    Normal,
    Large,
    XLarge,
    XxLarge,
}

impl ThumbSize {
    pub fn pixels(self) -> u32 {
        match self {
            Self::Normal => 128,
            Self::Large => 256,
            Self::XLarge => 512,
            Self::XxLarge => 1024,
        }
    }
    pub fn dir(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Large => "large",
            Self::XLarge => "x-large",
            Self::XxLarge => "xx-large",
        }
    }
}

pub fn cache_root() -> Option<PathBuf> {
    Some(dirs::cache_dir()?.join("thumbnails"))
}

/// True if `path` already lives inside the freedesktop thumbnail cache.
///
/// Such a file IS a thumbnail, so we must never generate a thumbnail for it:
/// the generated PNG would be written right back into the cache tree, and a
/// live folder watcher (when the user is browsing the cache itself) would then
/// treat that brand-new file as another source to thumbnail — a never-ending
/// generate/list/generate loop. Callers display these files directly instead.
pub fn is_in_thumbnail_cache(path: &Path) -> bool {
    let Some(root) = cache_root() else {
        return false;
    };
    // Canonicalize both sides so a symlinked $XDG_CACHE_HOME still matches.
    // Fall back to the raw paths when canonicalization fails (e.g. the file was
    // just deleted) so we still err on the side of detecting cache membership.
    let p = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let root = std::fs::canonicalize(&root).unwrap_or(root);
    p.starts_with(&root)
}

pub fn file_uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

pub fn md5_hex(s: &str) -> String {
    let mut h = Md5::new();
    h.update(s.as_bytes());
    let digest = h.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        write!(out, "{b:02x}").unwrap();
    }
    out
}

pub fn cache_path_for(uri: &str, size: ThumbSize) -> Option<PathBuf> {
    Some(cache_root()?.join(size.dir()).join(format!("{}.png", md5_hex(uri))))
}

pub fn fail_path_for(uri: &str, app_name: &str) -> Option<PathBuf> {
    Some(
        cache_root()?
            .join("fail")
            .join(app_name)
            .join(format!("{}.png", md5_hex(uri))),
    )
}

/// Marker recording that the source image is no larger than the `size`-px
/// thumbnail, so a thumbnail would just be a copy. Per-size because an image can
/// be small enough to skip at one size yet need shrinking at a larger one.
pub fn too_small_path_for(uri: &str, size: ThumbSize) -> Option<PathBuf> {
    Some(cache_root()?.join("too-small").join(size.dir()).join(md5_hex(uri)))
}

/// True if `thumb` exists and is at least as recent as the source's mtime.
/// (Strict spec-compliance would read the PNG tEXt `Thumb::MTime` chunk; the
/// mtime heuristic is good enough for now and matches most file managers.)
pub fn is_cache_fresh(thumb: &Path, source_mtime: SystemTime) -> bool {
    match std::fs::metadata(thumb).and_then(|m| m.modified()) {
        Ok(tm) => tm >= source_mtime,
        Err(_) => false,
    }
}

/// Decode an image file and write a `size`-px thumbnail to the freedesktop cache.
/// Returns the path to the cached thumbnail. Re-uses an existing fresh cache entry.
pub fn generate_image(src: &Path, size: ThumbSize) -> Result<PathBuf> {
    let canonical = std::fs::canonicalize(src).with_context(|| format!("canonicalize {}", src.display()))?;
    let uri = file_uri(&canonical);
    let target = cache_path_for(&uri, size).ok_or_else(|| anyhow!("no XDG cache dir"))?;

    let src_meta = std::fs::metadata(&canonical)?;
    let src_mtime = src_meta.modified()?;

    // Already known to be smaller than this thumbnail size → display the
    // original directly instead of decoding it again to (re)discover that a
    // thumbnail would just be a copy.
    if too_small_is_fresh(&canonical, size, src_mtime) {
        return Ok(canonical);
    }

    if is_cache_fresh(&target, src_mtime) {
        debug!(target = %target.display(), "cache hit");
        return Ok(target);
    }

    // Decode through the lower-level decoder API so we can read the EXIF
    // orientation tag and bake the rotation/flip into the pixels. `.decode()`
    // alone ignores orientation, which makes phone JPEGs (almost always stored
    // landscape with an orientation flag) show up rotated on their side.
    let reader = image::ImageReader::open(&canonical)
        .with_context(|| format!("open {}", canonical.display()))?
        .with_guessed_format()
        .with_context(|| format!("format-detect {}", canonical.display()))?;
    let mut decoder = reader
        .into_decoder()
        .with_context(|| format!("decoder {}", canonical.display()))?;

    let dim = size.pixels();

    // The image is no bigger than the thumbnail would be: a thumbnail can only
    // shrink, so it'd be a wasteful copy. Record a marker and hand back the
    // original so callers load it directly (cheap — it's a small image).
    let (w, h) = decoder.dimensions();
    if w <= dim && h <= dim {
        if let Err(err) = mark_too_small(&canonical, size) {
            warn!(?err, src = %canonical.display(), "could not write too-small marker");
        }
        debug!(src = %canonical.display(), w, h, "image smaller than thumbnail; using original");
        return Ok(canonical);
    }

    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img =
        image::DynamicImage::from_decoder(decoder).with_context(|| format!("decode {}", canonical.display()))?;
    img.apply_orientation(orientation);

    let thumb = img.thumbnail(dim, dim);
    thumb
        .to_rgba8()
        .save(&target)
        .with_context(|| format!("save {}", target.display()))?;

    debug!(target = %target.display(), source = %canonical.display(), "thumbnail written");
    Ok(target)
}

/// Dispatch thumbnail generation by extension/MIME.
/// On any failure marks the file in the freedesktop fail-cache.
pub fn generate_or_fail(src: &Path, size: ThumbSize, app_name: &str) -> Option<PathBuf> {
    // The file is itself a cached thumbnail: show it as-is rather than
    // generating a thumbnail-of-a-thumbnail (which would also loop via the
    // folder watcher when the cache directory is the one being viewed).
    if is_in_thumbnail_cache(src) {
        return Some(src.to_path_buf());
    }
    let result = match classify(src) {
        Kind::Image => generate_image(src, size),
        Kind::Pdf => generate_pdf(src, size),
        Kind::Video => generate_video(src, size),
        Kind::Audio => generate_audio(src, size),
        Kind::Other => return None,
    };
    match result {
        Ok(p) => Some(p),
        Err(err) => {
            warn!(?err, src = %src.display(), "thumbnail generation failed");
            let _ = mark_failed(src, app_name);
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Image,
    Pdf,
    Video,
    Audio,
    Other,
}

/// Extensions that mime_guess doesn't classify as `video/*` but ffmpegthumbnailer
/// can still extract a frame from. Keep lowercase.
const EXTRA_VIDEO_EXT: &[&str] = &["rmvb", "rm", "ts", "mts", "m2ts", "vob", "ogm", "divx", "f4v", "asf"];
/// Audio extensions we try to pull embedded album art from. Anything lofty
/// supports for picture frames (ID3v2 APIC, FLAC PICTURE, MP4 covr, Vorbis
/// METADATA_BLOCK_PICTURE) works here.
const AUDIO_EXT: &[&str] = &[
    "mp3", "flac", "m4a", "m4b", "mp4a", "aac", "ogg", "opus", "wav", "wv", "ape", "aiff", "aif",
];

fn classify(src: &Path) -> Kind {
    let mime = mime_guess::from_path(src).first();
    if let Some(m) = mime.as_ref().map(|m| m.essence_str()) {
        if m.starts_with("image/") {
            return Kind::Image;
        }
        if m.starts_with("video/") {
            return Kind::Video;
        }
        if m == "application/pdf" {
            return Kind::Pdf;
        }
        if m.starts_with("audio/") {
            return Kind::Audio;
        }
    }
    if let Some(ext) = src.extension().and_then(|e| e.to_str()) {
        let ext = ext.to_ascii_lowercase();
        if EXTRA_VIDEO_EXT.contains(&ext.as_str()) {
            return Kind::Video;
        }
        if AUDIO_EXT.contains(&ext.as_str()) {
            return Kind::Audio;
        }
    }
    Kind::Other
}

/// Render the first page of a PDF with hayro and downsize to a thumbnail.
pub fn generate_pdf(src: &Path, size: ThumbSize) -> Result<PathBuf> {
    use std::sync::Arc;

    use hayro::hayro_interpret::InterpreterSettings;
    use hayro::hayro_interpret::font::FontQuery;
    use hayro::hayro_syntax::Pdf;
    use hayro::vello_cpu::color::palette::css::WHITE;
    use hayro::{RenderCache, RenderSettings, render};

    let canonical = std::fs::canonicalize(src)?;
    let uri = file_uri(&canonical);
    let target = cache_path_for(&uri, size).ok_or_else(|| anyhow!("no XDG cache dir"))?;
    let src_mtime = std::fs::metadata(&canonical)?.modified()?;
    if is_cache_fresh(&target, src_mtime) {
        return Ok(target);
    }
    if let Some(p) = target.parent() {
        std::fs::create_dir_all(p)?;
    }

    let bytes = std::fs::read(&canonical)?;
    let pdf = Pdf::new(bytes).map_err(|e| anyhow!("pdf parse: {e:?}"))?;
    let pages = pdf.pages();
    let Some(first_page) = pages.first() else {
        return Err(anyhow!("pdf has no pages"));
    };

    let interp = InterpreterSettings {
        font_resolver: Arc::new(|query| match query {
            FontQuery::Standard(s) => Some(s.get_font_data()),
            FontQuery::Fallback(f) => Some(f.pick_standard_font().get_font_data()),
        }),
        ..Default::default()
    };

    // Scale the PDF render proportionally to the requested thumbnail size:
    // 128 → 0.25, 256 → 0.5, 512 → 1.0 (≈ 72 DPI), 1024 → 2.0. Without this
    // a US-letter page renders at ~153×198 regardless of target, which looks
    // pixelated when downsized into the XL gallery tile.
    let scale = (size.pixels() as f32 / 512.0).clamp(0.25, 2.0);
    let rs = RenderSettings {
        x_scale: scale,
        y_scale: scale,
        bg_color: WHITE,
        ..Default::default()
    };

    let cache = RenderCache::new();
    let pixmap = render(first_page, &cache, &interp, &rs);
    let w = pixmap.width() as u32;
    let h = pixmap.height() as u32;
    let rgba = pixmap.take_unpremultiplied();
    let mut buf = Vec::with_capacity((w * h * 4) as usize);
    for px in &rgba {
        buf.extend_from_slice(&[px.r, px.g, px.b, px.a]);
    }
    let img = image::RgbaImage::from_raw(w, h, buf).ok_or_else(|| anyhow!("rgba buffer size mismatch"))?;
    let dim = size.pixels();
    let thumb = image::DynamicImage::ImageRgba8(img).thumbnail(dim, dim);
    thumb.to_rgba8().save(&target)?;
    debug!(target = %target.display(), "pdf thumbnail written");
    Ok(target)
}

/// Extract embedded album art from an audio file via lofty and downsize to a
/// thumbnail. Returns `Err` if the file has no pictures or lofty can't read
/// it — caller marks as failed so we don't keep retrying.
pub fn generate_audio(src: &Path, size: ThumbSize) -> Result<PathBuf> {
    use lofty::file::TaggedFileExt;
    use lofty::picture::Picture;
    use lofty::probe::Probe;
    use lofty::tag::Accessor;

    let canonical = std::fs::canonicalize(src)?;
    let uri = file_uri(&canonical);
    let target = cache_path_for(&uri, size).ok_or_else(|| anyhow!("no XDG cache dir"))?;
    let src_mtime = std::fs::metadata(&canonical)?.modified()?;
    if is_cache_fresh(&target, src_mtime) {
        return Ok(target);
    }
    if let Some(p) = target.parent() {
        std::fs::create_dir_all(p)?;
    }

    let tagged = Probe::open(&canonical)
        .with_context(|| format!("lofty probe {}", canonical.display()))?
        .read()
        .with_context(|| format!("lofty read {}", canonical.display()))?;

    // Search through every tag block (ID3v2, MP4 ilst, Vorbis comments, etc.)
    // for the first picture frame. lofty exposes a flat .pictures() per tag,
    // and a file can carry several tags so we walk them all.
    let picture: Option<&Picture> = tagged.tags().iter().find_map(|t| {
        let _unused: &dyn Accessor = t; // ensure trait imported
        t.pictures().first()
    });
    let Some(pic) = picture else {
        return Err(anyhow!("no embedded picture in audio file"));
    };
    let data = pic.data();
    if data.is_empty() {
        return Err(anyhow!("embedded picture is empty"));
    }

    // The embedded picture is a normal image blob (JPEG/PNG/…). Hand it
    // straight to the `image` crate's auto-detect decoder so we don't depend
    // on the picture's declared MIME being accurate.
    let img = image::load_from_memory(data)
        .with_context(|| format!("decode embedded picture from {}", canonical.display()))?;
    let dim = size.pixels();
    let thumb = img.thumbnail(dim, dim);
    thumb
        .to_rgba8()
        .save(&target)
        .with_context(|| format!("save {}", target.display()))?;

    debug!(target = %target.display(), source = %canonical.display(), "audio thumbnail written");
    Ok(target)
}

/// Spawn `ffmpegthumbnailer` (if installed) to extract a video frame.
/// Returns `Err` if the tool is missing or fails — caller marks as failed.
pub fn generate_video(src: &Path, size: ThumbSize) -> Result<PathBuf> {
    let canonical = std::fs::canonicalize(src)?;
    let uri = file_uri(&canonical);
    let target = cache_path_for(&uri, size).ok_or_else(|| anyhow!("no XDG cache dir"))?;
    let src_mtime = std::fs::metadata(&canonical)?.modified()?;
    if is_cache_fresh(&target, src_mtime) {
        return Ok(target);
    }
    if let Some(p) = target.parent() {
        std::fs::create_dir_all(p)?;
    }

    let status = std::process::Command::new("ffmpegthumbnailer")
        .arg("-i")
        .arg(&canonical)
        .arg("-o")
        .arg(&target)
        .arg("-s")
        .arg(size.pixels().to_string())
        .arg("-q")
        .arg("8")
        .arg("-c")
        .arg("png")
        .arg("-t")
        .arg("10%")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    match status {
        Ok(s) if s.success() && target.exists() => {
            // Bake the film-strip overlay into the cached PNG so the gallery
            // can tell at a glance which thumbnails are videos.
            if let Err(err) = apply_film_strip(&target) {
                warn!(?err, target = %target.display(), "film strip overlay failed");
            }
            debug!(target = %target.display(), "video thumbnail written");
            Ok(target)
        }
        Ok(s) => Err(anyhow!("ffmpegthumbnailer exit status {s}")),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Err(anyhow!("ffmpegthumbnailer not installed")),
        Err(err) => Err(err.into()),
    }
}

/// Paint a film-strip border onto the freshly rendered video thumbnail:
/// solid black bars on the left + right edges with evenly-spaced light
/// "sprocket holes". Mirrors the Nemo / GNOME-Files convention so users
/// can tell videos apart from images at a glance.
fn apply_film_strip(target: &Path) -> Result<()> {
    let mut img = image::open(target)?.to_rgba8();
    let w = img.width();
    let h = img.height();
    if w < 24 || h < 24 {
        return Ok(()); // too small to make sense
    }

    // Strip width: ~9 % of image width, clamped to a sensible range.
    let strip_w = (w / 11).clamp(6, 28);
    let hole_w = (strip_w * 5 / 8).max(3);
    let hole_h = (h / 14).clamp(3, 14);
    // Hole count scales softly with aspect ratio. A linear h/w response
    // crushes landscape frames (2:1 → 5 holes — reads as random dots) and
    // overshoots tall portrait (2:1 → 20 — perfs touch each other). Using
    // a fractional power flattens both extremes:
    //   square (h/w=1.0)        → 10
    //   16:9 landscape (≈0.56)  →  8
    //   2:1 landscape (0.5)     →  8
    //   16:9 portrait (≈1.78)   → 12
    //   2:1 portrait (2.0)      → 12
    //   4:1 portrait (4.0)      → 15
    let aspect = (h as f32 / w as f32).max(0.05);
    let holes = ((10.0_f32 * aspect.powf(0.3)).round() as u32).clamp(4, 20);
    let hole_pad_x = (strip_w - hole_w) / 2;

    let black = image::Rgba([0, 0, 0, 255]);
    let hole_color = image::Rgba([230, 230, 230, 255]);

    // Left + right solid black bars.
    for y in 0..h {
        for x in 0..strip_w {
            img.put_pixel(x, y, black);
            img.put_pixel(w - 1 - x, y, black);
        }
    }

    // Sprocket holes — centred vertically in each (h / holes) band.
    for i in 0..holes {
        let center_y = (h * (2 * i + 1)) / (2 * holes);
        let y0 = center_y.saturating_sub(hole_h / 2);
        let y1 = (y0 + hole_h).min(h);
        for y in y0..y1 {
            for x in 0..hole_w {
                let lx = hole_pad_x + x;
                let rx = w - 1 - hole_pad_x - x;
                img.put_pixel(lx, y, hole_color);
                img.put_pixel(rx, y, hole_color);
            }
        }
    }

    img.save(target)?;
    Ok(())
}

fn mark_failed(src: &Path, app_name: &str) -> Result<()> {
    let canonical = std::fs::canonicalize(src)?;
    let uri = file_uri(&canonical);
    let p = fail_path_for(&uri, app_name).ok_or_else(|| anyhow!("no XDG cache dir"))?;
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&p, b"")?;
    Ok(())
}

fn mark_too_small(canonical: &Path, size: ThumbSize) -> Result<()> {
    let uri = file_uri(canonical);
    let p = too_small_path_for(&uri, size).ok_or_else(|| anyhow!("no XDG cache dir"))?;
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&p, b"")?;
    Ok(())
}

/// True if a fresh "too small" marker exists for this source + size, i.e. we
/// already determined the original should be displayed directly.
fn too_small_is_fresh(canonical: &Path, size: ThumbSize, src_mtime: SystemTime) -> bool {
    let uri = file_uri(canonical);
    let Some(p) = too_small_path_for(&uri, size) else {
        return false;
    };
    match std::fs::metadata(&p).and_then(|m| m.modified()) {
        Ok(tm) => tm >= src_mtime,
        Err(_) => false,
    }
}

pub fn is_marked_failed(src: &Path, app_name: &str) -> bool {
    let Ok(canonical) = std::fs::canonicalize(src) else {
        return false;
    };
    let Ok(src_mtime) = std::fs::metadata(&canonical).and_then(|m| m.modified()) else {
        return false;
    };
    let uri = file_uri(&canonical);
    let Some(p) = fail_path_for(&uri, app_name) else {
        return false;
    };
    match std::fs::metadata(&p).and_then(|m| m.modified()) {
        Ok(tm) => tm >= src_mtime,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn tmp() -> PathBuf {
        let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let p = std::env::temp_dir().join(format!("fm-test-thumb-{n:x}"));
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn md5_is_stable() {
        assert_eq!(md5_hex("file:///abc"), md5_hex("file:///abc"));
        assert_ne!(md5_hex("file:///a"), md5_hex("file:///b"));
    }

    #[test]
    fn cache_path_layout() {
        // We can't assert the exact prefix (depends on XDG_CACHE_HOME) but we
        // can verify the format of the leaf name and size dir.
        let p = cache_path_for("file:///x", ThumbSize::Normal).unwrap();
        assert!(p.parent().unwrap().ends_with("normal"));
        assert!(p.file_name().unwrap().to_string_lossy().ends_with(".png"));
    }

    #[test]
    fn cache_files_are_detected() {
        // A path under the cache root is recognised; an unrelated path is not.
        if let Some(root) = cache_root() {
            let inside = root.join("x-large").join("deadbeef.png");
            assert!(is_in_thumbnail_cache(&inside));
        }
        assert!(!is_in_thumbnail_cache(Path::new("/tmp/some/photo.jpg")));
    }

    #[test]
    fn generate_skips_files_already_in_cache() {
        // Generating for a file that lives in the cache must return that very
        // path (display-as-is) and must NOT create any sibling thumbnail.
        let Some(root) = cache_root() else { return };
        let dir = root.join("x-large");
        fs::create_dir_all(&dir).unwrap();
        let src = dir.join("fm-test-incache.png");
        image::RgbaImage::from_pixel(200, 200, image::Rgba([10, 20, 30, 255]))
            .save(&src)
            .unwrap();

        let out = generate_or_fail(&src, ThumbSize::XLarge, "fm-test");
        assert_eq!(out.as_deref(), Some(src.as_path()));

        let _ = fs::remove_file(&src);
    }

    #[test]
    fn generate_image_creates_thumbnail() {
        let dir = tmp();
        let src = dir.join("test.png");

        // Bigger than the 128 px thumbnail so it's actually downscaled (a
        // sub-128 px image would take the "too small, use original" path).
        let img = image::RgbaImage::from_pixel(300, 200, image::Rgba([200, 50, 50, 255]));
        img.save(&src).unwrap();

        let result = generate_image(&src, ThumbSize::Normal);
        assert!(result.is_ok(), "thumbnail generation failed: {result:?}");
        let thumb = result.unwrap();
        assert!(thumb.exists());
        assert_ne!(
            thumb,
            std::fs::canonicalize(&src).unwrap(),
            "should be a cached thumbnail, not the original"
        );
        let opened = image::ImageReader::open(&thumb).unwrap().decode().unwrap();
        assert!(opened.width() <= 128 && opened.height() <= 128);

        // Re-generate: should hit the cache (mtime ≥ source).
        let again = generate_image(&src, ThumbSize::Normal).unwrap();
        assert_eq!(again, thumb);

        let _ = fs::remove_dir_all(&dir);
        // Clean up cache entry so the test is self-contained.
        let _ = fs::remove_file(thumb);
    }

    #[test]
    fn small_image_uses_original_and_marks_it() {
        let dir = tmp();
        let src = dir.join("tiny.png");
        // 20×20 is well under any thumbnail size, so a thumbnail would be a
        // pointless copy.
        image::RgbaImage::from_pixel(20, 20, image::Rgba([1, 2, 3, 255]))
            .save(&src)
            .unwrap();
        let canonical = std::fs::canonicalize(&src).unwrap();
        let mtime = std::fs::metadata(&canonical).unwrap().modified().unwrap();

        let out = generate_image(&src, ThumbSize::Large).unwrap();
        assert_eq!(out, canonical, "too-small image should resolve to the original path");
        assert!(
            too_small_is_fresh(&canonical, ThumbSize::Large, mtime),
            "a marker should be recorded so the next call short-circuits"
        );

        let _ = fs::remove_dir_all(&dir);
        if let Some(p) = too_small_path_for(&file_uri(&canonical), ThumbSize::Large) {
            let _ = fs::remove_file(p);
        }
    }
}
