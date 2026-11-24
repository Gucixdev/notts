//! Small text helpers: fitting a message into a channel limit, escaping, base64, percent-encoding.

/// How a limit is counted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    /// Unicode scalar values.
    Chars,
    /// UTF-16 code units (what chat APIs usually mean by "characters"; emoji count 2).
    Utf16,
    /// UTF-8 bytes.
    Bytes,
}

impl Unit {
    pub fn cost(self, s: &str) -> usize {
        match self {
            Unit::Chars => s.chars().count(),
            Unit::Utf16 => s.encode_utf16().count(),
            Unit::Bytes => s.len(),
        }
    }
}

/// The visible marker appended when text is cut.
pub const MARKER: &str = " [...truncated]";

/// Escape one char into its replacement, or `None` to keep it.
pub type Esc = fn(char) -> Option<&'static str>;

pub fn no_esc(_: char) -> Option<&'static str> {
    None
}

pub fn esc_html(c: char) -> Option<&'static str> {
    match c {
        '&' => Some("&amp;"),
        '<' => Some("&lt;"),
        '>' => Some("&gt;"),
        _ => None,
    }
}

/// HTML for Matrix `formatted_body`: also quotes and turns newlines into `<br/>`.
pub fn esc_html_br(c: char) -> Option<&'static str> {
    match c {
        '&' => Some("&amp;"),
        '<' => Some("&lt;"),
        '>' => Some("&gt;"),
        '"' => Some("&quot;"),
        '\n' => Some("<br/>"),
        _ => None,
    }
}

/// Escape `s` and make the *escaped* result at most `budget` units; when text is cut, `MARKER`
/// is appended and counted. Never cuts inside an escape sequence or a char.
pub fn fit(s: &str, budget: usize, unit: Unit, esc: Esc) -> String {
    let escape_all = |s: &str| -> String {
        let mut o = String::with_capacity(s.len());
        for c in s.chars() {
            match esc(c) {
                Some(r) => o.push_str(r),
                None => o.push(c),
            }
        }
        o
    };
    let full = escape_all(s);
    if unit.cost(&full) <= budget {
        return full;
    }
    let marker = escape_all(MARKER);
    let room = budget.saturating_sub(unit.cost(&marker));
    let mut out = String::new();
    let mut used = 0;
    let mut buf = [0u8; 4];
    for c in s.chars() {
        let piece: &str = match esc(c) {
            Some(r) => r,
            None => c.encode_utf8(&mut buf),
        };
        let cost = unit.cost(piece);
        if used + cost > room {
            break;
        }
        out.push_str(piece);
        used += cost;
    }
    out.push_str(&marker);
    out
}

/// Plain truncation with the marker, counted in `unit`.
pub fn truncate(s: &str, budget: usize, unit: Unit) -> String {
    fit(s, budget, unit, no_esc)
}

pub fn pct_encode(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            o.push(b as char);
        } else {
            o.push_str(&format!("%{b:02X}"));
        }
    }
    o
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64(data: &[u8]) -> String {
    let mut o = String::with_capacity(data.len().div_ceil(3) * 4);
    for ch in data.chunks(3) {
        let n = (ch[0] as u32) << 16 | (*ch.get(1).unwrap_or(&0) as u32) << 8 | *ch.get(2).unwrap_or(&0) as u32;
        o.push(B64[(n >> 18) as usize & 63] as char);
        o.push(B64[(n >> 12) as usize & 63] as char);
        o.push(if ch.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        o.push(if ch.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    o
}

pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut acc = 0u32;
    let mut bits = 0;
    for c in s.bytes().filter(|b| !b.is_ascii_whitespace()) {
        if c == b'=' {
            break;
        }
        let v = B64.iter().position(|&x| x == c)? as u32;
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// FNV-1a 64: stable across Rust versions, which `DefaultHasher` is not.
pub fn fnv64(parts: &[&str]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for p in parts {
        for b in p.bytes().chain(std::iter::once(0u8)) {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
    }
    h
}

/// How `{name}` values are escaped when a template is rendered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TplMode {
    Json,
    Form,
    Raw,
}

impl TplMode {
    pub fn for_content_type(ct: &str) -> TplMode {
        let ct = ct.to_ascii_lowercase();
        if ct.contains("json") {
            TplMode::Json
        } else if ct.contains("x-www-form-urlencoded") {
            TplMode::Form
        } else {
            TplMode::Raw
        }
    }
}

/// Replace `{name}` with the escaped value in one pass (a value is never re-scanned).
/// Unknown `{names}` are left as they are.
pub fn render_template(tpl: &str, vars: &[(&str, &str)], mode: TplMode) -> String {
    let mut out = String::with_capacity(tpl.len());
    let mut rest = tpl;
    while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        if let Some(j) = after.find('}') {
            let name = &after[..j];
            if let Some((_, v)) = vars.iter().find(|(k, _)| *k == name) {
                match mode {
                    TplMode::Json => {
                        let q = serde_json::to_string(v).unwrap_or_default();
                        out.push_str(q.get(1..q.len().saturating_sub(1)).unwrap_or(""));
                    }
                    TplMode::Form => out.push_str(&pct_encode(v)),
                    TplMode::Raw => out.push_str(v),
                }
                rest = &after[j + 1..];
                continue;
            }
        }
        out.push('{');
        rest = after;
    }
    out.push_str(rest);
    out
}

/// SHA-256 (FIPS 180-4), for naming attachments in the history without storing their contents.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h: [u32; 8] = [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for (i, b) in chunk.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let mut v = h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7].wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v = [t1.wrapping_add(t2), v[0], v[1], v[2], v[3].wrapping_add(t1), v[4], v[5], v[6]];
        }
        for i in 0..8 {
            h[i] = h[i].wrapping_add(v[i]);
        }
    }
    let mut out = [0u8; 32];
    for (i, x) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&x.to_be_bytes());
    }
    out
}

pub fn sha256_hex(data: &[u8]) -> String {
    sha256(data).iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_keeps_short_text_and_marks_cut_text() {
        assert_eq!(truncate("abc", 10, Unit::Chars), "abc");
        let t = truncate(&"x".repeat(100), 40, Unit::Chars);
        assert_eq!(t.chars().count(), 40);
        assert!(t.ends_with(MARKER));
    }

    #[test]
    fn fit_counts_escaped_length_and_never_splits_an_entity() {
        let t = fit(&"<".repeat(50), 30, Unit::Chars, esc_html);
        assert!(t.chars().count() <= 30);
        assert!(!t.contains("&l&") && t.trim_end_matches(MARKER).len() % 4 == 0);
    }

    #[test]
    fn utf16_counts_emoji_twice() {
        let t = truncate(&"😀".repeat(100), 41, Unit::Utf16);
        assert!(t.encode_utf16().count() <= 41);
    }

    #[test]
    fn bytes_never_split_a_char() {
        let t = truncate(&"ł".repeat(100), 51, Unit::Bytes);
        assert!(t.len() <= 51);
        assert!(std::str::from_utf8(t.as_bytes()).is_ok());
    }

    #[test]
    fn base64_round_trips() {
        for s in ["", "a", "ab", "abc", "zażółć gęślą jaźń"] {
            assert_eq!(base64_decode(&base64(s.as_bytes())).unwrap(), s.as_bytes());
        }
        assert_eq!(base64(b"user:pass"), "dXNlcjpwYXNz");
    }

    #[test]
    fn template_escapes_per_mode_and_does_not_rescan() {
        let v = [("a", "x\"{b}"), ("b", "BAD")];
        assert_eq!(render_template("{\"k\":\"{a}\"}", &v, TplMode::Json), "{\"k\":\"x\\\"{b}\"}");
        assert_eq!(render_template("To={a}&z={nope}", &[("a", "+48 1")], TplMode::Form), "To=%2B48%201&z={nope}");
    }
}

/// Escape a field for the tab-separated state files: `\\`, tab, newline and CR.
pub fn escape_field(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            '\t' => o.push_str("\\t"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            c => o.push(c),
        }
    }
    o
}

pub fn unescape_field(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            o.push(c);
            continue;
        }
        match it.next() {
            Some('t') => o.push('\t'),
            Some('n') => o.push('\n'),
            Some('r') => o.push('\r'),
            Some('\\') => o.push('\\'),
            Some(x) => {
                o.push('\\');
                o.push(x)
            }
            None => o.push('\\'),
        }
    }
    o
}

#[cfg(test)]
mod field_tests {
    use super::*;

    #[test]
    fn fields_round_trip() {
        for s in ["", "a\tb", "line1\nline2\r\n", "back\\slash", "\\t literal", "zażółć 😀"] {
            let e = escape_field(s);
            assert!(!e.contains(['\t', '\n', '\r']));
            assert_eq!(unescape_field(&e), s);
        }
    }

    #[test]
    fn sha256_known_vectors() {
        assert_eq!(sha256_hex(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(sha256_hex(&vec![b'a'; 1000]), "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3");
    }
}
