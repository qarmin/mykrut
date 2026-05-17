//! Pattern-driven bulk rename.
//!
//! Syntax:
//! ```text
//! {idx}        — 1-based index in the selection
//! {N:idx}      — index padded to N characters with leading zeros (e.g. {3:idx} → "001")
//! {name}       — original filename without its extension
//! {ext}        — original extension without the leading dot ("" if none)
//! {ext.}       — original extension WITH the leading dot (".jpg" or "")
//! {{           — literal "{"
//! }}           — literal "}"
//! ```
//!
//! Any other text passes through verbatim. Unknown `{tokens}` are left as-is
//! so users get a visual hint when they typo a placeholder.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Literal(String),
    Idx { width: usize },
    Name,
    Ext,
    ExtDot,
}

/// Parse a pattern string into segments. Always succeeds — unknown placeholders
/// are kept as literal text so the live preview can show them.
#[expect(
    clippy::indexing_slicing,
    clippy::string_slice,
    reason = "byte/string indices are bounds-checked by the `while i < len` guard and the \
              `i + 1 < len` / `find` checks above each access; brace markers are ASCII so \
              `i`/`i + 1` are always on a char boundary"
)]
pub fn parse(pattern: &str) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let bytes = pattern.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        let b = bytes[i];

        if b == b'{' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            buf.push('{');
            i += 2;
            continue;
        }
        if b == b'}' && i + 1 < bytes.len() && bytes[i + 1] == b'}' {
            buf.push('}');
            i += 2;
            continue;
        }

        if b == b'{' {
            // Try to find the matching '}'.
            if let Some(end) = pattern[i + 1..].find('}') {
                let inner = &pattern[i + 1..i + 1 + end];
                if let Some(seg) = parse_placeholder(inner) {
                    if !buf.is_empty() {
                        out.push(Segment::Literal(std::mem::take(&mut buf)));
                    }
                    out.push(seg);
                    i += 1 + end + 1;
                    continue;
                }
                // Unknown placeholder → keep literal "{...}".
                buf.push('{');
                buf.push_str(inner);
                buf.push('}');
                i += 1 + end + 1;
                continue;
            }
        }

        // Not a brace marker — copy the next full UTF-8 character verbatim.
        // `i` is always on a char boundary here: brace branches only advance
        // past ASCII '{'/'}' and this branch advances by the char's byte length.
        match pattern[i..].chars().next() {
            Some(ch) => {
                buf.push(ch);
                i += ch.len_utf8();
            }
            None => break,
        }
    }

    if !buf.is_empty() {
        out.push(Segment::Literal(buf));
    }
    out
}

fn parse_placeholder(inner: &str) -> Option<Segment> {
    let inner = inner.trim();
    match inner {
        "idx" => Some(Segment::Idx { width: 0 }),
        "name" => Some(Segment::Name),
        "ext" => Some(Segment::Ext),
        "ext." => Some(Segment::ExtDot),
        _ => {
            // {N:idx}
            if let Some((w, rest)) = inner.split_once(':')
                && let Ok(width) = w.trim().parse::<usize>()
                && rest.trim() == "idx"
            {
                return Some(Segment::Idx { width });
            }
            None
        }
    }
}

/// Render the pattern for one entry. `idx` is 1-based.
pub fn render(segments: &[Segment], idx: usize, original_basename: &str) -> String {
    let (name, ext) = split_name_ext(original_basename);
    let mut out = String::new();
    for seg in segments {
        match seg {
            Segment::Literal(s) => out.push_str(s),
            Segment::Idx { width } => {
                if *width == 0 {
                    out.push_str(&idx.to_string());
                } else {
                    out.push_str(&format!("{:0>width$}", idx, width = *width));
                }
            }
            Segment::Name => out.push_str(name),
            Segment::Ext => out.push_str(ext),
            Segment::ExtDot => {
                if !ext.is_empty() {
                    out.push('.');
                    out.push_str(ext);
                }
            }
        }
    }
    out
}

/// Split "file.tar.gz" into ("file.tar", "gz"). Leading-dot files like ".env"
/// keep the dot in the name and have empty extension.
#[expect(
    clippy::string_slice,
    reason = "`idx` comes from `rfind('.')`, so it is a valid char boundary and `idx + 1 <= len`"
)]
fn split_name_ext(full: &str) -> (&str, &str) {
    if let Some(idx) = full.rfind('.')
        && idx > 0
    {
        return (&full[..idx], &full[idx + 1..]);
    }
    (full, "")
}

/// Apply pattern to a whole batch. Returns Vec<new_name> in input order.
pub fn render_batch(pattern: &str, originals: &[String]) -> Vec<String> {
    let segments = parse(pattern);
    originals
        .iter()
        .enumerate()
        .map(|(i, name)| render(&segments, i + 1, name))
        .collect()
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::literal_string_with_formatting_args,
        reason = "`{N:idx}` etc. are rename-DSL placeholders inside test patterns, not format-string args"
    )]

    use super::*;

    #[test]
    fn parses_literals() {
        assert_eq!(parse("hello"), vec![Segment::Literal("hello".into())]);
    }

    #[test]
    fn parses_idx_no_width() {
        let s = parse("file_{idx}");
        assert_eq!(s, vec![Segment::Literal("file_".into()), Segment::Idx { width: 0 }]);
    }

    #[test]
    fn parses_idx_with_width() {
        let s = parse("img_{3:idx}.jpg");
        assert_eq!(
            s,
            vec![
                Segment::Literal("img_".into()),
                Segment::Idx { width: 3 },
                Segment::Literal(".jpg".into())
            ]
        );
    }

    #[test]
    fn parses_name_and_ext() {
        let s = parse("{name}_backup{ext.}");
        assert_eq!(
            s,
            vec![Segment::Name, Segment::Literal("_backup".into()), Segment::ExtDot]
        );
    }

    #[test]
    fn renders_index_padding() {
        let segs = parse("{3:idx}_x");
        assert_eq!(render(&segs, 1, "anything"), "001_x");
        assert_eq!(render(&segs, 42, "anything"), "042_x");
        assert_eq!(render(&segs, 1234, "anything"), "1234_x");
    }

    #[test]
    fn renders_name_and_ext() {
        let segs = parse("{name}_v2{ext.}");
        assert_eq!(render(&segs, 1, "report.pdf"), "report_v2.pdf");
        assert_eq!(render(&segs, 1, "noext"), "noext_v2");
    }

    #[test]
    fn escapes_braces() {
        let segs = parse("{{literal}}");
        assert_eq!(render(&segs, 1, "x"), "{literal}");
    }

    #[test]
    fn unknown_placeholder_passes_through() {
        let segs = parse("{wat}_{idx}");
        assert_eq!(render(&segs, 1, "x"), "{wat}_1");
    }

    #[test]
    fn preserves_non_ascii_literals() {
        // Regression: literal text was previously copied byte-by-byte as
        // `b as char`, mojibake-ing any multi-byte UTF-8 in the pattern.
        let segs = parse("café-{idx}-日本");
        assert_eq!(render(&segs, 1, "x"), "café-1-日本");
        let segs = parse("{name}_ÄÖÜ");
        assert_eq!(render(&segs, 1, "report.pdf"), "report_ÄÖÜ");
    }

    #[test]
    fn batch_renders_in_order() {
        let names: Vec<String> = vec!["a.jpg".into(), "b.png".into(), "c.gif".into()];
        let r = render_batch("photo_{2:idx}{ext.}", &names);
        assert_eq!(r, vec!["photo_01.jpg", "photo_02.png", "photo_03.gif"]);
    }
}
