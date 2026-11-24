//! E-mail over SMTP, std sockets plus rustls for TLS (`security=tls` implicit TLS on 465,
//! `security=starttls` on 587, `security=none` plain, only for localhost or test relays).
//! AUTH PLAIN is only sent over TLS (or to a loopback host, or with insecure_auth=yes).
//! Mail is `text/plain; charset=utf-8`, base64 body, RFC 2047 subject: Polish letters and emoji survive.

use super::*;
use crate::text::{base64, truncate, Unit};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;

pub const LIMIT: usize = 100_000;

pub struct Email {
    host: String,
    port: u16,
    security: Security,
    auth: Option<(String, Secret)>,
    from: String,
    to: Vec<String>,
    insecure_auth: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Security {
    None,
    StartTls,
    Tls,
}

fn addr_ok(a: &str) -> bool {
    !a.is_empty() && !a.contains(['\r', '\n', '<', '>', ' ']) && a.contains('@')
}

pub fn build(o: &Opts) -> Result<Box<dyn Channel>, String> {
    let host = o.need("host")?.to_string();
    let security = match o.get("security").unwrap_or("starttls") {
        "none" => Security::None,
        "starttls" => Security::StartTls,
        "tls" => Security::Tls,
        _ => return Err("security must be none, starttls or tls".into()),
    };
    let port = match o.get("port") {
        Some(p) => p.parse().map_err(|_| "port must be a number".to_string())?,
        None => match security {
            Security::Tls => 465,
            Security::StartTls => 587,
            Security::None => 25,
        },
    };
    let from = o.need("from")?.to_string();
    let to: Vec<String> = o.need("to")?.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    if !addr_ok(&from) || to.is_empty() || !to.iter().all(|t| addr_ok(t)) {
        return Err("`from` and `to` must be plain addresses like name@host".into());
    }
    let auth = match o.get("user") {
        Some(u) => Some((u.to_string(), o.secret("pass")?)),
        None => None,
    };
    Ok(Box::new(Email { host, port, security, auth, from, to, insecure_auth: o.get("insecure_auth") == Some("yes") }))
}

fn rfc2047(s: &str) -> String {
    if s.is_ascii() && !s.contains("=?") {
        return s.to_string();
    }
    // encoded words are limited to 75 chars; split on char boundaries
    let mut words = Vec::new();
    let mut cur = String::new();
    for c in s.chars() {
        if cur.len() + c.len_utf8() > 42 {
            words.push(std::mem::take(&mut cur));
        }
        cur.push(c);
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words.iter().map(|w| format!("=?UTF-8?B?{}?=", base64(w.as_bytes()))).collect::<Vec<_>>().join("\r\n ")
}

impl Email {
    pub fn compose(&self, m: &Message, now_unix: u64) -> String {
        let mut text = m.body.clone();
        if let Some(l) = &m.link {
            text.push_str(&format!("\n\n{l}"));
        }
        let text = truncate(&text, LIMIT, Unit::Chars).replace("\r\n", "\n").replace('\n', "\r\n");
        let wrap = |b: &[u8]| -> String { base64(b).as_bytes().chunks(76).map(|c| std::str::from_utf8(c).unwrap_or("")).collect::<Vec<_>>().join("\r\n") };
        let subject = format!("[{}] {}", m.severity.as_str(), if m.title.is_empty() { "notification" } else { &m.title });
        let mut head = format!(
            "From: {}\r\nTo: {}\r\nSubject: {}\r\nDate: {}\r\nMessage-ID: <{}.{}@notts>\r\nX-Notts-App: {}\r\nMIME-Version: 1.0\r\n",
            self.from,
            self.to.join(", "),
            rfc2047(&subject),
            rfc2822(now_unix),
            m.id,
            now_unix,
            m.app.replace(['\r', '\n'], " "),
        );
        if m.attachments.is_empty() {
            return format!("{head}Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\n{}\r\n", wrap(text.as_bytes()));
        }
        let boundary = format!("notts-{}", &crate::text::sha256_hex(m.id.as_bytes())[..20]);
        head.push_str(&format!("Content-Type: multipart/mixed; boundary=\"{boundary}\"\r\n\r\nThis is a multi-part message in MIME format.\r\n"));
        head.push_str(&format!("--{boundary}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\n{}\r\n", wrap(text.as_bytes())));
        for a in &m.attachments {
            let ascii: String = a.name.chars().map(|c| if c.is_ascii() { c } else { '_' }).collect();
            head.push_str(&format!(
                "--{boundary}\r\nContent-Type: {}; name=\"{ascii}\"\r\nContent-Transfer-Encoding: base64\r\nContent-Disposition: attachment; filename=\"{ascii}\"; filename*=UTF-8''{}\r\n\r\n{}\r\n",
                content_type_of(&a.name).split(';').next().unwrap_or("application/octet-stream"),
                crate::text::pct_encode(&a.name),
                wrap(&a.data)
            ));
        }
        head.push_str(&format!("--{boundary}--\r\n"));
        head
    }

    fn connect(&self, timeout: Duration) -> Result<TcpStream, SendError> {
        let addrs = (self.host.as_str(), self.port).to_socket_addrs().map_err(|_| SendError::retryable("host not found"))?;
        let mut last = SendError::retryable("connection failed");
        for a in addrs {
            match TcpStream::connect_timeout(&a, timeout) {
                Ok(s) => {
                    let _ = s.set_read_timeout(Some(timeout));
                    let _ = s.set_write_timeout(Some(timeout));
                    return Ok(s);
                }
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => last = SendError::retryable("timeout"),
                Err(_) => last = SendError::retryable("connection failed"),
            }
        }
        Err(last)
    }
}

trait Stream: Read + Write {}
impl<T: Read + Write> Stream for T {}

fn io_err(e: std::io::Error) -> SendError {
    match e.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => SendError::retryable("timeout"),
        k => SendError::retryable(format!("network error ({k:?})")),
    }
}

/// One SMTP reply: code and the last line.
fn read_reply(r: &mut BufReader<Box<dyn Stream>>) -> Result<(u16, String), SendError> {
    let mut last;
    loop {
        let mut line = String::new();
        let n = r.by_ref().take(4096).read_line(&mut line).map_err(io_err)?;
        if n == 0 {
            return Err(SendError::retryable("connection closed by server"));
        }
        let line = line.trim_end().to_string();
        let code: u16 = line.get(..3).and_then(|c| c.parse().ok()).ok_or_else(|| SendError::Permanent("malformed SMTP reply".into()))?;
        let more = line.as_bytes().get(3) == Some(&b'-');
        last = (code, line);
        if !more {
            return Ok(last);
        }
    }
}

fn expect(r: &mut BufReader<Box<dyn Stream>>, ok: &[u16], what: &str) -> Result<String, SendError> {
    let (code, line) = read_reply(r)?;
    if ok.contains(&code) {
        Ok(line)
    } else if (400..500).contains(&code) {
        Err(SendError::retryable(format!("SMTP {code} at {what}")))
    } else {
        Err(SendError::Permanent(format!("SMTP {code} at {what}")))
    }
}

fn send_line(r: &mut BufReader<Box<dyn Stream>>, s: &str) -> Result<(), SendError> {
    let w = r.get_mut();
    w.write_all(s.as_bytes()).and_then(|_| w.write_all(b"\r\n")).and_then(|_| w.flush()).map_err(io_err)
}

fn tls_wrap(tcp: TcpStream, host: &str) -> Result<Box<dyn Stream>, SendError> {
    let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|_| SendError::Permanent("tls setup failed".into()))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from(host.to_string()).map_err(|_| SendError::Permanent("invalid host name for tls".into()))?;
    let conn = rustls::ClientConnection::new(Arc::new(cfg), name).map_err(|_| SendError::Permanent("tls setup failed".into()))?;
    Ok(Box::new(rustls::StreamOwned::new(conn, tcp)))
}

fn is_loopback(host: &str) -> bool {
    host == "localhost" || host.parse::<std::net::IpAddr>().is_ok_and(|a| a.is_loopback())
}

impl Channel for Email {
    fn kind(&self) -> &'static str {
        "email"
    }
    fn capacity(&self) -> Option<Capacity> {
        Some(Capacity { limit: LIMIT, max_parts: 5 })
    }
    fn cost(&self, m: &Message) -> usize {
        m.body.chars().count() + m.link.as_ref().map_or(0, |l| l.chars().count() + 2)
    }
    fn files(&self) -> Option<FileCaps> {
        Some(FileCaps { max_bytes: 8 * 1024 * 1024, with_text: true })
    }
    fn describe(&self, m: &Message) -> String {
        format!("SMTP {}:{} ({:?}) mail to {} recipient(s), subject {} chars", self.host, self.port, self.security, self.to.len(), m.title.chars().count())
    }
    fn deliver(&self, m: &Message, ctx: &Ctx) -> Result<(), SendError> {
        let tcp = self.connect(ctx.timeout)?;
        let spare = tcp.try_clone().map_err(io_err)?;
        let stream: Box<dyn Stream> = if self.security == Security::Tls { tls_wrap(tcp, &self.host)? } else { Box::new(tcp) };
        let mut c = BufReader::new(stream);
        expect(&mut c, &[220], "greeting")?;
        send_line(&mut c, "EHLO notts.local")?;
        expect(&mut c, &[250], "EHLO")?;
        if self.security == Security::StartTls {
            send_line(&mut c, "STARTTLS")?;
            expect(&mut c, &[220], "STARTTLS")?;
            // nothing is buffered after the 220: the server waits for our ClientHello
            c = BufReader::new(tls_wrap(spare, &self.host)?);
            send_line(&mut c, "EHLO notts.local")?;
            expect(&mut c, &[250], "EHLO")?;
        }
        if let Some((user, pass)) = &self.auth {
            if self.security == Security::None && !self.insecure_auth && !is_loopback(&self.host) {
                return Err(SendError::Permanent("refusing to send a password without TLS (use security=tls|starttls or insecure_auth=yes)".into()));
            }
            let cred = base64(format!("\0{user}\0{}", pass.expose()).as_bytes());
            send_line(&mut c, &format!("AUTH PLAIN {cred}"))?;
            expect(&mut c, &[235], "AUTH")?;
        }
        send_line(&mut c, &format!("MAIL FROM:<{}>", self.from))?;
        expect(&mut c, &[250], "MAIL FROM")?;
        for t in &self.to {
            send_line(&mut c, &format!("RCPT TO:<{t}>"))?;
            expect(&mut c, &[250, 251], "RCPT TO")?;
        }
        send_line(&mut c, "DATA")?;
        expect(&mut c, &[354], "DATA")?;
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let mail = self.compose(m, now);
        // headers and base64 never start a line with '.', so no dot-stuffing is needed
        let w = c.get_mut();
        w.write_all(mail.as_bytes()).and_then(|_| w.write_all(b".\r\n")).and_then(|_| w.flush()).map_err(io_err)?;
        expect(&mut c, &[250], "end of DATA")?;
        let _ = send_line(&mut c, "QUIT");
        Ok(())
    }
    fn secrets(&self) -> Vec<String> {
        let mut v: Vec<String> = self.to.clone();
        v.push(self.from.clone());
        if let Some((u, p)) = &self.auth {
            v.push(u.clone());
            v.push(p.expose().to_string());
        }
        v
    }
}

fn rfc2822(unix: u64) -> String {
    let days = (unix / 86400) as i64;
    let secs = unix % 86400;
    // civil from days (Howard Hinnant)
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mth <= 2 { y + 1 } else { y };
    let wd = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"][(days.rem_euclid(7)) as usize];
    let mon = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"][(mth - 1) as usize];
    format!("{wd}, {d} {mon} {y} {:02}:{:02}:{:02} +0000", secs / 3600, secs % 3600 / 60, secs % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_is_rfc2822() {
        assert_eq!(rfc2822(0), "Thu, 1 Jan 1970 00:00:00 +0000");
        assert_eq!(rfc2822(1_795_554_802), "Tue, 24 Nov 2026 21:13:22 +0000");
    }

    #[test]
    fn subject_with_unicode_is_encoded_words() {
        let s = rfc2047("[fail] Zażółć gęślą jaźń — dysk pełny 😀");
        assert!(s.starts_with("=?UTF-8?B?"));
        assert!(s.is_ascii());
        assert_eq!(rfc2047("plain"), "plain");
    }
}
