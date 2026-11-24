//! The digest: held messages collected per app, sent as ONE message per period.

use crate::config::{Config, DigestCfg, Schedule};
use crate::message::{Message, Severity};
use crate::policy::hhmm;
use crate::state::{Pending, State};

/// Most recent scheduled time (unix) at or before `now`, for `at=` schedules.
fn last_occurrence(times: &[u32], now: u64, local_minutes: u32) -> Option<u64> {
    let midnight = now.checked_sub(local_minutes as u64 * 60 + now % 60)?;
    let mut best: Option<u64> = None;
    for day_back in 0..=1u64 {
        let day0 = midnight.checked_sub(day_back * 86_400);
        for t in times {
            if let Some(d0) = day0 {
                let occ = d0 + *t as u64 * 60;
                if occ <= now && best.is_none_or(|b| occ > b) {
                    best = Some(occ);
                }
            }
        }
    }
    best
}

/// Is a digest due? Never when nothing is pending.
pub fn due(cfg: &Config, st: &State, now: u64, local_minutes: u32) -> bool {
    let Some(d) = &cfg.digest else { return false };
    let Some(first) = st.pending.values().map(|p| p.first.min(now)).min() else { return false };
    // a last-digest time in the future means the clock went back: count from now
    let last = st.last_digest.map(|l| l.min(now));
    match &d.schedule {
        Schedule::Every(secs) => {
            let base = last.unwrap_or(0).max(first);
            now >= base + secs
        }
        Schedule::At(times) => match last_occurrence(times, now, local_minutes) {
            Some(occ) => occ > last.unwrap_or(0) && occ >= first,
            None => false,
        },
    }
}

/// Build the digest message and the channels it goes to. `None` for an empty digest.
pub fn compose(d: &DigestCfg, rows: &[Pending], now: u64, local_minutes: u32) -> Option<(Message, Vec<String>)> {
    if rows.is_empty() {
        return None;
    }
    let total: u64 = rows.iter().map(|r| r.count).sum();
    let worst = rows.iter().map(|r| r.severity).max().unwrap_or(Severity::Info);
    let mut apps: Vec<(String, Vec<&Pending>)> = Vec::new();
    for r in rows {
        match apps.iter_mut().find(|(a, _)| *a == r.app) {
            Some((_, v)) => v.push(r),
            None => apps.push((r.app.clone(), vec![r])),
        }
    }
    for (_, v) in apps.iter_mut() {
        v.sort_by(|a, b| b.severity.cmp(&a.severity).then(b.count.cmp(&a.count)).then(a.title.cmp(&b.title)));
    }
    apps.sort_by(|(an, a), (bn, b)| {
        let w = |v: &Vec<&Pending>| v.iter().map(|r| r.severity).max();
        w(b).cmp(&w(a)).then(b.iter().map(|r| r.count).sum::<u64>().cmp(&a.iter().map(|r| r.count).sum::<u64>())).then(an.cmp(bn))
    });

    let mut body = String::new();
    let mut shown = 0usize;
    let mut shown_msgs = 0u64;
    let mut lines_left = d.lines;
    'outer: for (app, v) in &apps {
        let n: u64 = v.iter().map(|r| r.count).sum();
        let worst_app = v.iter().map(|r| r.severity).max().unwrap_or(Severity::Info);
        if lines_left < 2 {
            break;
        }
        body.push_str(&format!("{app}: {n} message(s), worst {}\n", worst_app.as_str()));
        lines_left -= 1;
        for r in v {
            if lines_left == 0 {
                break 'outer;
            }
            body.push_str(&format!("  {} x{} {} (since {})\n", r.severity.as_str(), r.count, r.title, hhmm(r.first, now, local_minutes)));
            lines_left -= 1;
            shown += 1;
            shown_msgs += r.count;
        }
    }
    if shown < rows.len() {
        body.push_str(&format!("... and {} more ({} message(s) in {} more group(s))\n", rows.len() - shown, total - shown_msgs, rows.len() - shown));
    }
    let napps = apps.len();
    let mut channels: Vec<String> = d.to.clone();
    if channels.is_empty() {
        for r in rows {
            for c in &r.channels {
                if !channels.contains(c) {
                    channels.push(c.clone());
                }
            }
        }
    }
    let msg = Message::new(format!("Digest: {total} message(s) from {napps} app(s)")).app("notts").tag("digest").severity(worst).body(body.trim_end());
    Some((msg, channels))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn at_schedule_finds_last_occurrence_across_midnight() {
        // now = day 10, 00:30 local (UTC offset 0); at 23:00 -> yesterday 23:00
        let now = 10 * 86_400 + 30 * 60;
        assert_eq!(last_occurrence(&[23 * 60], now, 30), Some(9 * 86_400 + 23 * 3600));
        assert_eq!(last_occurrence(&[0], now, 30), Some(10 * 86_400));
        assert_eq!(last_occurrence(&[8 * 60, 18 * 60], now, 30), Some(9 * 86_400 + 18 * 3600));
    }
}
