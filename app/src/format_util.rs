use std::time::{SystemTime, UNIX_EPOCH};

pub fn human_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if v >= 100.0 {
        format!("{:.0} {}", v, UNITS[i])
    } else if v >= 10.0 {
        format!("{:.1} {}", v, UNITS[i])
    } else {
        format!("{:.2} {}", v, UNITS[i])
    }
}

/// Minimal "YYYY-MM-DD HH:MM" formatter — no chrono dep.
/// Good enough for MVP; replace with `time` crate later if locale matters.
pub fn human_mtime(t: SystemTime) -> String {
    let Ok(dur) = t.duration_since(UNIX_EPOCH) else {
        return String::new();
    };
    let total = dur.as_secs() as i64;
    let (y, mo, d, h, mi) = epoch_to_ymdhm(total);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}")
}

// Convert a UNIX timestamp to (year, month, day, hour, minute) in UTC.
// Algorithm: Howard Hinnant's days_from_civil (public domain).
fn epoch_to_ymdhm(secs: i64) -> (i32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let day_secs = secs.rem_euclid(86_400) as u32;
    let h = day_secs / 3600;
    let m = (day_secs % 3600) / 60;

    let z = days + 719_468; // shift epoch to 0000-03-01
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y_proleptic = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = (y_proleptic + i64::from(month <= 2)) as i32;
    (year, month, day, h, m)
}

pub fn kind_text(mime: Option<&str>, is_dir: bool) -> String {
    if is_dir {
        return "Folder".to_string();
    }
    match mime {
        Some(m) => m.to_string(),
        None => "File".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_formats_round_correctly() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.00 KB");
        assert_eq!(human_size(1024 * 1024), "1.00 MB");
    }

    #[test]
    fn mtime_unix_epoch() {
        assert_eq!(human_mtime(UNIX_EPOCH), "1970-01-01 00:00");
    }
}
