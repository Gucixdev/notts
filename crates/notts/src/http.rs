//! Blocking HTTP with a hard timeout and one place that decides what an answer means.

use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn new(method: &str, url: impl Into<String>) -> Request {
        Request { method: method.to_string(), url: url.into(), headers: vec![], body: vec![] }
    }
    pub fn header(mut self, k: &str, v: impl Into<String>) -> Self {
        self.headers.push((k.to_string(), v.into()));
        self
    }
    pub fn body(mut self, b: impl Into<Vec<u8>>) -> Self {
        self.body = b.into();
        self
    }
    /// `POST https://host:port/***` for dry runs: the path may hold a token.
    pub fn describe(&self) -> String {
        let origin = match self.url.find("://") {
            Some(i) => {
                let rest = &self.url[i + 3..];
                let host_end = rest.find(['/', '?']).unwrap_or(rest.len());
                // drop any user:pass@
                let host = rest[..host_end].rsplit('@').next().unwrap_or("");
                format!("{}://{}", &self.url[..i], host)
            }
            None => "?".to_string(),
        };
        let mut s = format!("{} {origin}/***", self.method);
        for (k, _) in &self.headers {
            s.push_str(&format!(" [{k}]"));
        }
        s.push_str(&format!(" body={}B", self.body.len()));
        if let Ok(t) = std::str::from_utf8(&self.body) {
            s.push_str(&format!(" {}", t.replace('\n', "\\n")));
        }
        s
    }
}

/// Why a delivery attempt did not succeed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendError {
    /// Worth trying again (network, timeout, 5xx, 429). `after` is the server's Retry-After.
    Retryable { detail: String, after: Option<Duration> },
    /// Retrying will not help (bad token, 4xx, bad config).
    Permanent(String),
}

impl SendError {
    pub fn detail(&self) -> &str {
        match self {
            SendError::Retryable { detail, .. } => detail,
            SendError::Permanent(d) => d,
        }
    }
    pub fn retryable(detail: impl Into<String>) -> SendError {
        SendError::Retryable { detail: detail.into(), after: None }
    }
}

pub struct Http {
    agent: ureq::Agent,
}

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub retry_after: Option<Duration>,
    pub body: String,
}

impl Http {
    /// `env_proxy`: honour HTTP(S)_PROXY / ALL_PROXY. Tests pass false.
    pub fn new(timeout: Duration, env_proxy: bool) -> Http {
        let mut b = ureq::Agent::config_builder().timeout_global(Some(timeout)).http_status_as_error(false).max_redirects(0).user_agent(concat!("notts/", env!("CARGO_PKG_VERSION")));
        if !env_proxy {
            b = b.proxy(None);
        }
        Http { agent: b.build().into() }
    }

    pub fn call(&self, r: &Request) -> Result<Response, SendError> {
        let mut b = ureq::http::Request::builder().method(r.method.as_str()).uri(&r.url);
        for (k, v) in &r.headers {
            b = b.header(k, v);
        }
        let req = b.body(r.body.clone()).map_err(|_| SendError::Permanent("invalid request (bad url or header)".into()))?;
        let mut resp = self.agent.run(req).map_err(map_err)?;
        let status = resp.status().as_u16();
        let header_secs = resp.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<f64>().ok());
        let body = resp.body_mut().with_config().limit(65_536).lossy_utf8(true).read_to_string().unwrap_or_default();
        let retry_after = header_secs.or_else(|| retry_after_from_body(&body)).filter(|s| s.is_finite() && *s >= 0.0).map(|s| Duration::from_millis((s.min(86_400.0) * 1000.0) as u64));
        Ok(Response { status, retry_after, body })
    }

    /// Send and judge: 2xx is success.
    pub fn execute(&self, r: &Request) -> Result<(), SendError> {
        classify(self.call(r)?)
    }
}

/// Telegram `parameters.retry_after` (s), Discord `retry_after` (s), Matrix `retry_after_ms`.
fn retry_after_from_body(body: &str) -> Option<f64> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    if let Some(ms) = v.get("retry_after_ms").and_then(|x| x.as_f64()) {
        return Some(ms / 1000.0);
    }
    v.get("retry_after").or_else(|| v.get("parameters").and_then(|p| p.get("retry_after"))).and_then(|x| x.as_f64())
}

pub fn classify(r: Response) -> Result<(), SendError> {
    match r.status {
        200..=299 => Ok(()),
        429 => Err(SendError::Retryable { detail: "HTTP 429 rate limited".into(), after: r.retry_after }),
        408 | 500..=599 => Err(SendError::Retryable { detail: format!("HTTP {}", r.status), after: r.retry_after }),
        s => Err(SendError::Permanent(format!("HTTP {s}"))),
    }
}

fn map_err(e: ureq::Error) -> SendError {
    use ureq::Error as E;
    match e {
        E::Timeout(_) => SendError::retryable("timeout"),
        E::HostNotFound => SendError::retryable("host not found"),
        E::ConnectionFailed => SendError::retryable("connection failed"),
        E::Io(io) => SendError::retryable(format!("network error ({:?})", io.kind())),
        E::BadUri(_) | E::InvalidProxyUrl | E::Http(_) => SendError::Permanent("invalid url or request".into()),
        E::Tls(_) | E::Rustls(_) => SendError::Permanent("tls error".into()),
        _ => SendError::retryable("transport error"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_hides_path_and_userinfo() {
        let r = Request::new("POST", "https://user:pw@example.com:8443/bot123:SECRET/send?x=1").header("Authorization", "Bearer SECRET").body("hi");
        let d = r.describe();
        assert!(d.starts_with("POST https://example.com:8443/***"), "{d}");
        assert!(!d.contains("SECRET") && !d.contains("pw"));
    }

    #[test]
    fn classify_statuses() {
        let r = |s| Response { status: s, retry_after: Some(Duration::from_secs(3)), body: String::new() };
        assert!(classify(r(204)).is_ok());
        assert!(matches!(classify(r(429)), Err(SendError::Retryable { after: Some(_), .. })));
        assert!(matches!(classify(r(503)), Err(SendError::Retryable { .. })));
        assert!(matches!(classify(r(401)), Err(SendError::Permanent(_))));
        assert!(matches!(classify(r(302)), Err(SendError::Permanent(_))));
    }

    #[test]
    fn retry_after_body_forms() {
        assert_eq!(retry_after_from_body("{\"parameters\":{\"retry_after\":7}}"), Some(7.0));
        assert_eq!(retry_after_from_body("{\"retry_after\":1.5}"), Some(1.5));
        assert_eq!(retry_after_from_body("{\"errcode\":\"M_LIMIT_EXCEEDED\",\"retry_after_ms\":2500}"), Some(2.5));
        assert_eq!(retry_after_from_body("not json"), None);
    }
}
