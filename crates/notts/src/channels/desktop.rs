//! Local desktop notification through `notify-send` (or `cmd=` of your choice), no shell involved.
//! The app name is always `notts`, which `notts watch` ignores, so this sink cannot loop.
//! If finedesktop's notification daemon owns org.freedesktop.Notifications, this shows up there
//! like any other notification; with the finedesktop `on-notify` hook set up, do not add this
//! channel to rules for the same messages (it would not loop, but you would see each twice).

use super::*;
use std::process::{Command, Stdio};
use std::time::Instant;

pub struct Desktop {
    cmd: String,
}

pub fn build(o: &Opts) -> Result<Box<dyn Channel>, String> {
    Ok(Box::new(Desktop { cmd: o.get("cmd").unwrap_or("notify-send").to_string() }))
}

impl Desktop {
    fn args(&self, m: &Message) -> Vec<String> {
        use crate::message::Severity::*;
        let urgency = match m.severity {
            Fail => "critical",
            Warn => "normal",
            _ => "low",
        };
        let mut body = m.body.clone();
        if let Some(l) = &m.link {
            body.push('\n');
            body.push_str(l);
        }
        vec!["-a".into(), "notts".into(), "-u".into(), urgency.into(), "--".into(), format!("{} {}", m.severity.emoji(), m.title), crate::text::truncate(&body, 2000, crate::text::Unit::Chars)]
    }
}

impl Channel for Desktop {
    fn kind(&self) -> &'static str {
        "desktop"
    }
    fn describe(&self, m: &Message) -> String {
        format!("run {} -a notts ... ({} chars)", self.cmd, m.title.chars().count() + m.body.chars().count())
    }
    fn deliver(&self, m: &Message, ctx: &Ctx) -> Result<(), SendError> {
        let mut child = Command::new(&self.cmd)
            .args(self.args(m))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| SendError::Permanent(format!("cannot run {} ({:?})", self.cmd, e.kind())))?;
        let start = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(st)) if st.success() => return Ok(()),
                Ok(Some(st)) => return Err(SendError::Permanent(format!("{} exited with {}", self.cmd, st.code().unwrap_or(-1)))),
                Ok(None) if start.elapsed() > ctx.timeout => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(SendError::retryable("timeout"));
                }
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
                Err(_) => return Err(SendError::retryable("cannot wait for notifier")),
            }
        }
    }
    fn secrets(&self) -> Vec<String> {
        vec![]
    }
}
