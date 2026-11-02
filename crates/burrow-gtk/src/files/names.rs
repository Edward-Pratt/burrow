//! Names and paths from the server: checked before use, cleaned before they are shown.

use std::path::Path;

/// "Replace …?" for a name that is already there.
pub(super) fn replace_question(
    name: &str,
    is_dir: bool,
    place: &str,
) -> (String, &'static str, &'static str) {
    let name = printable(name);
    if is_dir {
        (
            format!("Merge into the {place} “{name}”?"),
            "A folder with this name is already there. Files in it with the same names are replaced.",
            "Merge",
        )
    } else {
        (
            format!("Replace “{name}”?"),
            "Something with this name is already there, and it will be overwritten.",
            "Replace",
        )
    }
}

/// A name that is safe to put after a folder: on the server, and above all
/// on this computer, where `..` would leave the folder the user picked.
pub(super) fn checked_name(name: &str) -> Result<&str, String> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
        Err(format!(
            "“{}” can't be used as a file name.",
            printable(name)
        ))
    } else {
        Ok(name)
    }
}

/// The name a local file gets on the server. Remote paths are text, so a
/// name that isn't UTF-8 can't be sent.
pub(super) fn upload_name(local: &Path) -> Option<String> {
    local.file_name()?.to_str().map(String::from)
}

/// `name` inside the remote folder `dir`.
pub(super) fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// The folder holding `path`, or `None` at the root.
pub(super) fn parent(path: &str) -> Option<String> {
    let path = path.trim_end_matches('/');
    let cut = path.rfind('/')?;
    Some(if cut == 0 { "/" } else { &path[..cut] }.to_string())
}

/// A name or error from the server, with control characters replaced so
/// they can't change what is around them.
pub(crate) fn printable(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { '\u{fffd}' } else { c })
        .collect()
}

/// `rwxr-xr-x` for `0o755`.
pub(super) fn mode(bits: u32) -> String {
    (0..9)
        .map(|i| {
            if bits & (1 << (8 - i)) == 0 {
                '-'
            } else {
                ['r', 'w', 'x'][i % 3]
            }
        })
        .collect()
}

/// "1.5 MiB".
pub(crate) fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_join_and_climb() {
        assert_eq!(join("/", "etc"), "/etc");
        assert_eq!(join("/home/me", "notes"), "/home/me/notes");
        assert_eq!(parent("/home/me"), Some("/home".into()));
        assert_eq!(parent("/home"), Some("/".into()));
        assert_eq!(parent("/home/me/"), Some("/home".into()));
        assert_eq!(parent("/"), None);
        assert_eq!(parent(""), None);
    }

    #[test]
    fn unsafe_names_are_refused() {
        for name in ["", ".", "..", "a/b", "nul\0"] {
            assert!(checked_name(name).is_err(), "{name:?}");
        }
        assert_eq!(checked_name("..hidden"), Ok("..hidden"));
    }

    #[test]
    fn control_characters_are_not_shown() {
        assert_eq!(printable("a\x1b[2Jb\nc"), "a\u{fffd}[2Jb\u{fffd}c");
    }

    #[test]
    fn sizes_and_modes_read_well() {
        assert_eq!(size(512), "512 B");
        assert_eq!(size(1536), "1.5 KiB");
        assert_eq!(size(20 * 1024 * 1024), "20 MiB");
        assert_eq!(mode(0o755), "rwxr-xr-x");
        assert_eq!(mode(0o640), "rw-r-----");
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_names_are_not_uploaded() {
        use std::os::unix::ffi::OsStrExt;
        let path = Path::new(std::ffi::OsStr::from_bytes(b"/tmp/bad\xff"));
        assert_eq!(upload_name(path), None);
        assert_eq!(upload_name(Path::new("/tmp/good")), Some("good".into()));
    }
}
