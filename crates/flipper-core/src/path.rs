//! Validation and normalization for paths sent to the Flipper,
//! mirroring `FlipperPath` in the iOS FlipperKit.

use crate::error::{Error, Result};

pub struct FlipperPath;

impl FlipperPath {
    pub const ROOTS: [&'static str; 3] = ["/ext", "/int", "/any"];
    pub const MAX_LENGTH: usize = 255;

    /// Returns a normalized absolute path ("." and "//" collapsed) or an error.
    /// ".." is rejected outright instead of being resolved.
    pub fn normalize(raw: &str) -> Result<String> {
        if raw.is_empty() {
            return Err(Error::InvalidPath("empty".into()));
        }
        if raw.len() > Self::MAX_LENGTH {
            return Err(Error::InvalidPath("too long".into()));
        }
        if !raw.starts_with('/') {
            return Err(Error::InvalidPath("must be absolute".into()));
        }
        if raw.chars().any(|c| c.is_control()) {
            return Err(Error::InvalidPath("control character".into()));
        }
        let mut parts: Vec<&str> = Vec::new();
        for part in raw.split('/') {
            match part {
                "" | "." => continue,
                ".." => return Err(Error::InvalidPath("'..' is not allowed".into())),
                _ => parts.push(part),
            }
        }
        let Some(first) = parts.first() else {
            return Err(Error::InvalidPath(
                "must be under /ext, /int or /any".into(),
            ));
        };
        let root = format!("/{first}");
        if !Self::ROOTS.contains(&root.as_str()) {
            return Err(Error::InvalidPath(
                "must be under /ext, /int or /any".into(),
            ));
        }
        Ok(format!("/{}", parts.join("/")))
    }

    pub fn last_component(path: &str) -> &str {
        path.rsplit('/').next().unwrap_or(path)
    }

    pub fn parent(path: &str) -> String {
        let mut parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        parts.pop();
        format!("/{}", parts.join("/"))
    }

    pub fn join(dir: &str, name: &str) -> String {
        if dir.ends_with('/') {
            format!("{dir}{name}")
        } else {
            format!("{dir}/{name}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(raw: &str) -> Error {
        FlipperPath::normalize(raw).unwrap_err()
    }

    #[test]
    fn normalizes() {
        assert_eq!(FlipperPath::normalize("/ext").unwrap(), "/ext");
        assert_eq!(FlipperPath::normalize("/ext/").unwrap(), "/ext");
        assert_eq!(
            FlipperPath::normalize("//ext//a///b//").unwrap(),
            "/ext/a/b"
        );
        assert_eq!(FlipperPath::normalize("/ext/./a/./b").unwrap(), "/ext/a/b");
        assert_eq!(FlipperPath::normalize("/int/x/sub").unwrap(), "/int/x/sub");
        assert_eq!(
            FlipperPath::normalize("/any/file.txt").unwrap(),
            "/any/file.txt"
        );
    }

    #[test]
    fn rejects_bad_paths() {
        assert!(matches!(err(""), Error::InvalidPath(_)));
        assert!(matches!(err("ext/x"), Error::InvalidPath(_)));
        assert!(matches!(err("/"), Error::InvalidPath(_)));
        assert!(matches!(err("/sd/x"), Error::InvalidPath(_)));
        assert!(matches!(err("/int/../ext/x"), Error::InvalidPath(_)));
        assert!(matches!(err("/ext/a\0b"), Error::InvalidPath(_)));
        let long = format!("/ext/{}", "a".repeat(300));
        assert!(matches!(err(&long), Error::InvalidPath(_)));
    }

    #[test]
    fn components() {
        assert_eq!(
            FlipperPath::last_component("/ext/apps/file.sub"),
            "file.sub"
        );
        assert_eq!(FlipperPath::last_component("/ext"), "ext");
        assert_eq!(FlipperPath::parent("/ext/apps/file.sub"), "/ext/apps");
        assert_eq!(FlipperPath::parent("/ext"), "/");
        assert_eq!(FlipperPath::join("/ext", "a.sub"), "/ext/a.sub");
        assert_eq!(FlipperPath::join("/ext/", "a.sub"), "/ext/a.sub");
    }
}
