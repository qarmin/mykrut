//! Pure-Rust media metadata probing for the Properties dialog.
//!
//! Cheap, header-only reads:
//! * **Image** — pixel dimensions (`image::image_dimensions`) + a curated set of
//!   EXIF tags (`kamadak-exif`).
//! * **Audio / Video** — tags + stream properties via `lofty` (covers the common
//!   containers it supports; MP4-family video yields duration/tags too).
//!
//! Anything we can't read leaves the row list empty, and the caller then hides
//! the media section rather than showing a blank tab.

use std::path::Path;
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MediaKind {
    None,
    Image,
    Audio,
    Video,
}

impl MediaKind {
    /// Human label used as the Properties media-tab title.
    pub fn label(self) -> &'static str {
        match self {
            Self::Image => "Image",
            Self::Audio => "Audio",
            Self::Video => "Video",
            Self::None => "",
        }
    }
}

pub struct MediaInfo {
    pub kind: MediaKind,
    /// (label, value) rows to display in the dialog.
    pub rows: Vec<(String, String)>,
}

/// Probe `path` for type-specific metadata. `mime` (if known) drives
/// classification; otherwise it's guessed from the extension.
pub fn probe(path: &Path, mime: Option<&str>) -> MediaInfo {
    let class = classify(path, mime);
    let rows = match class {
        MediaKind::Image => image_rows(path),
        MediaKind::Audio | MediaKind::Video => tag_rows(path),
        MediaKind::None => Vec::new(),
    };
    // Nothing useful extracted → report None so the UI hides the section.
    let kind = if rows.is_empty() { MediaKind::None } else { class };
    MediaInfo { kind, rows }
}

fn classify(path: &Path, mime: Option<&str>) -> MediaKind {
    let from = |m: &str| {
        if m.starts_with("image/") {
            MediaKind::Image
        } else if m.starts_with("audio/") {
            MediaKind::Audio
        } else if m.starts_with("video/") {
            MediaKind::Video
        } else {
            MediaKind::None
        }
    };
    if let Some(m) = mime {
        let k = from(m);
        if k != MediaKind::None {
            return k;
        }
    }
    if let Some(g) = mime_guess::from_path(path).first() {
        return from(g.essence_str());
    }
    MediaKind::None
}

fn image_rows(path: &Path) -> Vec<(String, String)> {
    let mut rows = Vec::new();
    if let Ok((w, h)) = image::image_dimensions(path) {
        rows.push(("Dimensions".to_string(), format!("{w} × {h}")));
    }
    if let Ok(file) = std::fs::File::open(path) {
        let mut buf = std::io::BufReader::new(&file);
        if let Ok(exif) = exif::Reader::new().read_from_container(&mut buf) {
            use exif::{In, Tag};
            let wanted = [
                (Tag::Make, "Camera make"),
                (Tag::Model, "Camera model"),
                (Tag::LensModel, "Lens"),
                (Tag::DateTimeOriginal, "Taken"),
                (Tag::ExposureTime, "Exposure"),
                (Tag::FNumber, "Aperture"),
                (Tag::PhotographicSensitivity, "ISO"),
                (Tag::FocalLength, "Focal length"),
                (Tag::Orientation, "Orientation"),
                (Tag::Software, "Software"),
            ];
            for (tag, label) in wanted {
                if let Some(field) = exif.get_field(tag, In::PRIMARY) {
                    let value = field.display_value().with_unit(&exif).to_string();
                    let value = value.trim().trim_matches('"').to_string();
                    if !value.is_empty() {
                        rows.push((label.to_string(), value));
                    }
                }
            }
        }
    }
    rows
}

fn tag_rows(path: &Path) -> Vec<(String, String)> {
    use lofty::file::{AudioFile, TaggedFileExt};
    use lofty::probe::Probe;
    use lofty::tag::Accessor;

    let mut rows = Vec::new();
    let Ok(tagged) = Probe::open(path).and_then(|p| p.read()) else {
        return rows;
    };

    let props = tagged.properties();
    if props.duration() > Duration::ZERO {
        rows.push(("Length".to_string(), fmt_duration(props.duration())));
    }
    if let Some(br) = props.audio_bitrate() {
        rows.push(("Bitrate".to_string(), format!("{br} kbps")));
    }
    if let Some(sr) = props.sample_rate() {
        rows.push(("Sample rate".to_string(), format!("{:.1} kHz", f64::from(sr) / 1000.0)));
    }
    if let Some(ch) = props.channels() {
        rows.push(("Channels".to_string(), ch.to_string()));
    }

    if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
        let mut push = |label: &str, value: Option<std::borrow::Cow<'_, str>>| {
            if let Some(v) = value {
                let v = v.trim();
                if !v.is_empty() {
                    rows.push((label.to_string(), v.to_string()));
                }
            }
        };
        push("Title", tag.title());
        push("Artist", tag.artist());
        push("Album", tag.album());
        push("Genre", tag.genre());
    }
    rows
}

fn fmt_duration(d: Duration) -> String {
    let total = d.as_secs();
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_uses_mime_then_extension() {
        assert_eq!(classify(Path::new("x.bin"), Some("image/png")), MediaKind::Image);
        assert_eq!(classify(Path::new("song.mp3"), None), MediaKind::Audio);
        assert_eq!(classify(Path::new("clip.mp4"), None), MediaKind::Video);
        assert_eq!(classify(Path::new("notes.txt"), None), MediaKind::None);
    }

    #[test]
    fn duration_formats() {
        assert_eq!(fmt_duration(Duration::from_secs(5)), "0:05");
        assert_eq!(fmt_duration(Duration::from_secs(125)), "2:05");
        assert_eq!(fmt_duration(Duration::from_secs(3725)), "1:02:05");
    }

    #[test]
    fn probe_text_file_is_none() {
        let info = probe(Path::new("/etc/hostname"), Some("text/plain"));
        assert_eq!(info.kind, MediaKind::None);
        assert!(info.rows.is_empty());
    }
}
