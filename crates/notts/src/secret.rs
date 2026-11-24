//! Secrets: read from a mode-600 file or an environment variable, never from the config text,
//! and a [`Masker`] that removes them (and other private values) from anything printed.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

/// A secret value. `Debug` never shows it.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn new(v: impl Into<String>) -> Secret {
        Secret(v.into())
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// `~/x` becomes `$HOME/x`.
pub fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(h) = std::env::var_os("HOME") {
            return PathBuf::from(h).join(rest);
        }
    }
    PathBuf::from(p)
}

/// Read a secret file. Refuses anything group- or world-accessible. The error text names the
/// path (not a secret) and never the content.
pub fn read_secret_file(path: &str) -> Result<Secret, String> {
    let p = expand_tilde(path);
    let shown = p.display();
    let meta = std::fs::metadata(&p).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => format!("secret file {shown} not found"),
        std::io::ErrorKind::PermissionDenied => format!("secret file {shown} not readable"),
        _ => format!("secret file {shown} unreadable ({:?})", e.kind()),
    })?;
    if !meta.is_file() {
        return Err(format!("secret path {shown} is not a regular file"));
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(format!("secret file {shown} is group/world accessible (mode {mode:03o}); run: chmod 600 {shown}"));
    }
    let raw = std::fs::read(&p).map_err(|e| format!("secret file {shown} unreadable ({:?})", e.kind()))?;
    let text = String::from_utf8_lossy(&raw);
    let v = text.trim_end_matches(['\n', '\r']).trim().to_string();
    if v.is_empty() {
        return Err(format!("secret file {shown} is empty"));
    }
    Ok(Secret(v))
}

/// Read a secret from an environment variable (looked up through `env`).
pub fn read_secret_env(name: &str, env: &dyn Fn(&str) -> Option<String>) -> Result<Secret, String> {
    match env(name) {
        Some(v) if !v.trim().is_empty() => Ok(Secret(v.trim().to_string())),
        _ => Err(format!("environment variable {name} is not set")),
    }
}

const MIN_MASK_LEN: usize = 4;

/// Replaces every registered private value by `***`. Values shorter than 4 chars are not
/// registered: masking them would shred ordinary text.
#[derive(Clone, Default)]
pub struct Masker {
    vals: Vec<String>,
}

impl Masker {
    pub fn new() -> Masker {
        Masker::default()
    }
    pub fn add(&mut self, v: &str) {
        let v = v.trim();
        if v.chars().count() < MIN_MASK_LEN {
            return;
        }
        for form in [v.to_string(), crate::text::pct_encode(v), crate::text::base64(v.as_bytes())] {
            if !self.vals.contains(&form) {
                self.vals.push(form);
            }
        }
        self.vals.sort_by_key(|s| std::cmp::Reverse(s.len()));
    }
    pub fn mask(&self, s: &str) -> String {
        let mut out = s.to_string();
        for v in &self.vals {
            if out.contains(v.as_str()) {
                out = out.replace(v.as_str(), "***");
            }
        }
        out
    }
}

impl std::fmt::Debug for Masker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Masker({} values)", self.vals.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("notts-secret-{}-{name}", std::process::id()));
        d
    }

    fn write(name: &str, mode: u32, content: &str) -> PathBuf {
        let p = tmp(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
        p
    }

    #[test]
    fn mode_600_is_read_and_trimmed() {
        let p = write("ok", 0o600, "tok-123456\n");
        assert_eq!(read_secret_file(p.to_str().unwrap()).unwrap().expose(), "tok-123456");
    }

    #[test]
    fn group_or_world_readable_is_refused_with_a_clear_message_and_no_content() {
        for mode in [0o640, 0o644, 0o604, 0o660] {
            let p = write(&format!("m{mode:o}"), mode, "supersecretvalue");
            let e = read_secret_file(p.to_str().unwrap()).unwrap_err();
            assert!(e.contains("chmod 600"), "{e}");
            assert!(!e.contains("supersecretvalue"));
        }
    }

    #[test]
    fn missing_and_empty_are_errors() {
        assert!(read_secret_file("/nonexistent/notts/x").unwrap_err().contains("not found"));
        let p = write("empty", 0o600, "\n");
        assert!(read_secret_file(p.to_str().unwrap()).unwrap_err().contains("empty"));
    }

    #[test]
    fn env_secret() {
        let env = |n: &str| if n == "A" { Some("val-1234".to_string()) } else { None };
        assert_eq!(read_secret_env("A", &env).unwrap().expose(), "val-1234");
        assert!(read_secret_env("B", &env).unwrap_err().contains("B"));
    }

    #[test]
    fn masker_hides_plain_encoded_and_ignores_short_values() {
        let mut m = Masker::new();
        m.add("tok/en+1234");
        m.add("12");
        let s = format!("x {} y {} z 12", "tok/en+1234", crate::text::pct_encode("tok/en+1234"));
        assert_eq!(m.mask(&s), "x *** y *** z 12");
        assert_eq!(format!("{:?}", Secret::new("hunter22")), "Secret(***)");
    }
}
