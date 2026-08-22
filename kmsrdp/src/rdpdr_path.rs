//! Windows-style path helpers for the RDPDR FUSE bridge.

/// True when `name` is a single path component safe to join onto a
/// Windows-style RDPDR path. Rejects empty, `.`/`..`, and names that
/// contain separators or NULs.
pub fn is_safe_win_component(name: &str) -> bool {
    if name.is_empty() || name == "." || name == ".." {
        return false;
    }
    !name
        .chars()
        .any(|c| c == '\\' || c == '/' || c == '\0' || c == ':')
}

pub fn sanitize_dos_name(raw: &str) -> String {
    let trimmed = raw.trim_matches(|c: char| c == '\0' || c.is_whitespace());
    trimmed
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub fn join_win(parent: &str, name: &str) -> Option<String> {
    if !is_safe_win_component(name) {
        return None;
    }
    Some(if parent == "\\" {
        format!("\\{name}")
    } else {
        format!("{parent}\\{name}")
    })
}

pub fn parent_of(path: &str) -> String {
    if path == "\\" {
        return "\\".to_owned();
    }
    match path.rsplit_once('\\') {
        Some(("", _)) => "\\".to_owned(),
        Some((parent, _)) => parent.to_owned(),
        None => "\\".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_win_and_parent_of() {
        assert_eq!(join_win("\\", "foo").as_deref(), Some("\\foo"));
        assert_eq!(join_win("\\dir", "bar").as_deref(), Some("\\dir\\bar"));
        assert_eq!(parent_of("\\"), "\\");
        assert_eq!(parent_of("\\foo"), "\\");
        assert_eq!(parent_of("\\dir\\file"), "\\dir");
    }

    #[test]
    fn join_win_rejects_traversal_and_separators() {
        assert_eq!(join_win("\\", ".."), None);
        assert_eq!(join_win("\\", "."), None);
        assert_eq!(join_win("\\", ""), None);
        assert_eq!(join_win("\\", "a\\b"), None);
        assert_eq!(join_win("\\", "a/b"), None);
        assert_eq!(join_win("\\", "C:evil"), None);
    }

    #[test]
    fn sanitize_dos_name_strips_and_replaces() {
        assert_eq!(sanitize_dos_name("  C  "), "C");
        assert_eq!(sanitize_dos_name(" my-drive "), "my-drive");
        assert_eq!(sanitize_dos_name("foo/bar"), "foo_bar");
    }
}
