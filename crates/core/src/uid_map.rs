//! UID/GID → name resolver via /etc/passwd + /etc/group, lazily cached.
//!
//! Pure Rust — no `libc::getpwuid` bindings. The file is read once on first
//! access, then never again (Unix user databases are effectively static for a
//! running process).

use std::collections::HashMap;
use std::sync::OnceLock;

static USERS: OnceLock<HashMap<u32, String>> = OnceLock::new();
static GROUPS: OnceLock<HashMap<u32, String>> = OnceLock::new();

fn parse_passwd_like(text: &str) -> HashMap<u32, String> {
    text.lines()
        .filter_map(|line| {
            let mut parts = line.split(':');
            let name = parts.next()?.to_string();
            let _passwd = parts.next()?;
            let id: u32 = parts.next()?.parse().ok()?;
            Some((id, name))
        })
        .collect()
}

fn load(path: &str) -> HashMap<u32, String> {
    std::fs::read_to_string(path)
        .map(|t| parse_passwd_like(&t))
        .unwrap_or_default()
}

pub fn user_name(uid: u32) -> Option<String> {
    USERS.get_or_init(|| load("/etc/passwd")).get(&uid).cloned()
}

pub fn group_name(gid: u32) -> Option<String> {
    GROUPS.get_or_init(|| load("/etc/group")).get(&gid).cloned()
}

/// Format as "name (uid)" if resolvable, else just "uid".
pub fn format_user(uid: u32) -> String {
    match user_name(uid) {
        Some(name) => format!("{name} ({uid})"),
        None => uid.to_string(),
    }
}

pub fn format_group(gid: u32) -> String {
    match group_name(gid) {
        Some(name) => format!("{name} ({gid})"),
        None => gid.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_passwd_format() {
        let text = "root:x:0:0:root:/root:/bin/bash\n\
                    daemon:x:1:1:daemon:/usr/sbin:/usr/sbin/nologin\n\
                    bogus_line_with_no_colons\n\
                    user:x:1000:1000:Real Name,,,:/home/user:/bin/bash\n";
        let m = parse_passwd_like(text);
        assert_eq!(m.get(&0).map(String::as_str), Some("root"));
        assert_eq!(m.get(&1).map(String::as_str), Some("daemon"));
        assert_eq!(m.get(&1000).map(String::as_str), Some("user"));
        assert_eq!(m.len(), 3);
    }

    #[test]
    fn format_user_falls_back_to_uid() {
        // 999999 almost certainly not in /etc/passwd
        let s = format_user(999_999);
        assert_eq!(s, "999999");
    }
}
