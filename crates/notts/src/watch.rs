//! `notts watch`: forward desktop notifications (org.freedesktop.Notifications.Notify calls seen on
//! the session bus) to the remote channels. Passive: it listens through `busctl --user monitor`
//! and never owns a bus name, so it coexists with finedesktop's own notification daemon.

use crate::message::{Message, Severity};

/// One `Notify` call out of a `busctl monitor --json=short` line; `None` for anything else.
pub fn parse_notify(line: &str) -> Option<Message> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if v.get("type")?.as_str()? != "method_call" || v.get("member")?.as_str()? != "Notify" || v.get("interface")?.as_str()? != "org.freedesktop.Notifications" {
        return None;
    }
    let data = v.get("payload")?.get("data")?.as_array()?;
    let app = data.first()?.as_str()?;
    let summary = data.get(3)?.as_str()?;
    let body = data.get(4)?.as_str().unwrap_or("");
    let urgency = data.get(6).and_then(|h| h.get("urgency")).and_then(|u| u.get("data")).and_then(|d| d.as_u64()).unwrap_or(1);
    // the desktop sink of notts itself: never forward it again
    if app == "notts" {
        return None;
    }
    let sev = match urgency {
        2 => Severity::Fail,
        _ => Severity::Info,
    };
    Some(Message::new(summary).body(body).app(app).severity(sev).tag("desktop"))
}

/// Map the urgency words of finedesktop's `on-notify` hook (FD_URGENCY) to a severity.
pub fn severity_from_urgency(u: &str) -> Severity {
    match u {
        "critical" | "2" => Severity::Fail,
        _ => Severity::Info,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINE: &str = r#"{"type":"method_call","endian":"l","flags":0,"version":1,"cookie":9,"timestamp-realtime":1795554802593595,"sender":":1.1","destination":":1.2","path":"/org/freedesktop/Notifications","interface":"org.freedesktop.Notifications","member":"Notify","payload":{"type":"susssasa{sv}i","data":["testapp",0,"","Zażółć tytuł","line1\nline2 \"q\"",[],{"urgency":{"type":"y","data":2},"sender-pid":{"type":"x","data":527475}},-1]}}"#;

    #[test]
    fn parses_a_real_busctl_line() {
        let m = parse_notify(LINE).unwrap();
        assert_eq!((m.app.as_str(), m.title.as_str(), m.body.as_str()), ("testapp", "Zażółć tytuł", "line1\nline2 \"q\""));
        assert_eq!(m.severity, Severity::Fail);
    }

    #[test]
    fn ignores_other_calls_garbage_and_its_own_sink() {
        assert!(parse_notify("not json").is_none());
        assert!(parse_notify(r#"{"type":"method_call","member":"GetServerInformation","interface":"org.freedesktop.Notifications","payload":{"data":[]}}"#).is_none());
        assert!(parse_notify(&LINE.replace("\"testapp\"", "\"notts\"")).is_none());
        assert!(parse_notify(r#"{"type":"method_return"}"#).is_none());
    }
}
