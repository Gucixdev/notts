//! How one message is carried by one channel: whole, split on line boundaries at the channel's real
//! limit, with the file attached natively, or as a summary plus a pointer to the saved full report.
//! Nothing is ever cut without a visible marker that says where the rest is.

use crate::channels::Channel;
use crate::message::{Attachment, Message};

pub struct Plan {
    /// Deliveries, in order.
    pub parts: Vec<Message>,
    /// `message`, `split`, `attachment`, `link` or `truncated`.
    pub how: &'static str,
    /// Text parts only (a file sent on its own does not count).
    pub text_parts: u32,
}

/// The id suffix of a delivery that carries only a file; such a message is already planned.
pub const FILE_PART: &str = ".f";

/// `save(file_name, bytes)` stores a copy and returns where (or why not); only called when a pointer is needed.
pub fn plan(c: &dyn Channel, msg: &Message, save: &dyn Fn(&str, &[u8]) -> String) -> Plan {
    if msg.id.ends_with(FILE_PART) {
        return Plan { parts: vec![msg.clone()], how: "attachment", text_parts: 0 };
    }
    let mut base = msg.clone();
    let atts: Vec<Attachment> = std::mem::take(&mut base.attachments);
    let mut how = "message";
    let mut file_mode: Option<bool> = None;
    let mut pointer: Option<String> = None;
    if !atts.is_empty() {
        match c.files() {
            Some(fc) if atts.iter().all(|a| a.data.len() <= fc.max_bytes) => file_mode = Some(fc.with_text),
            _ => {
                let paths: Vec<String> = atts.iter().map(|a| a.path.clone().unwrap_or_else(|| save(&a.name, &a.data))).collect();
                let note = format!("full report: {}", paths.join(", "));
                base.body = if base.body.is_empty() { note } else { format!("{}\n\n{note}", base.body) };
                pointer = paths.first().cloned();
                how = "link";
            }
        }
    }
    let (mut parts, capped) = split(c, &base, pointer, save);
    if capped {
        how = "truncated";
    } else if how == "message" && parts.len() > 1 {
        how = "split";
    }
    let text_parts = parts.len() as u32;
    if let Some(with_text) = file_mode {
        if with_text {
            if let Some(last) = parts.last_mut() {
                last.attachments = atts;
            }
        } else {
            let mut f = Message::new(base.title.clone()).severity(base.severity).app(base.app.clone()).tag(base.tag.clone()).with_id(format!("{}{FILE_PART}", base.id));
            f.attachments = atts;
            parts.push(f);
        }
        if !capped {
            how = "attachment";
        }
    }
    Plan { parts, how, text_parts }
}

fn split(c: &dyn Channel, base: &Message, pointer: Option<String>, save: &dyn Fn(&str, &[u8]) -> String) -> (Vec<Message>, bool) {
    let Some(cap) = c.capacity() else { return (vec![base.clone()], false) };
    if c.cost(base) <= cap.limit {
        return (vec![base.clone()], false);
    }
    let probe = |body: &str| -> Message {
        let mut p = base.clone();
        p.title = if base.title.is_empty() { "(99/99)".into() } else { format!("{} (99/99)", base.title) };
        p.body = body.to_string();
        p
    };
    // fixed part of a message (title, link, separators) = the cost of one body char minus that char
    let (c1, c2) = (c.cost(&probe("a")), c.cost(&probe("aa")));
    let floor = c1.saturating_sub(c2.saturating_sub(c1));
    if cap.limit <= floor + 1 {
        // the title and link alone fill a message: nothing sensible to split, the channel marks its own cut
        return (vec![base.clone()], false);
    }
    let room = cap.limit - floor;
    let cost = |s: &str| c.cost(&probe(s)).saturating_sub(floor);
    let mut bodies: Vec<String> = Vec::new();
    let (mut cur, mut cur_cost) = (String::new(), 0usize);
    for atom in base.body.split_inclusive('\n') {
        let ac = cost(atom);
        if ac > room {
            // one line longer than a message: cut it by characters, never inside one
            if !cur.is_empty() {
                bodies.push(std::mem::take(&mut cur));
                cur_cost = 0;
            }
            let mut buf = [0u8; 4];
            for ch in atom.chars() {
                let cc = cost(ch.encode_utf8(&mut buf));
                if cur_cost + cc > room && !cur.is_empty() {
                    bodies.push(std::mem::take(&mut cur));
                    cur_cost = 0;
                }
                cur.push(ch);
                cur_cost += cc;
            }
            continue;
        }
        if cur_cost + ac > room && !cur.is_empty() {
            bodies.push(std::mem::take(&mut cur));
            cur_cost = 0;
        }
        cur.push_str(atom);
        cur_cost += ac;
    }
    if !cur.is_empty() {
        bodies.push(cur);
    }
    for b in &mut bodies {
        let t = b.trim_end_matches('\n').len();
        b.truncate(t);
    }
    if bodies.len() <= 1 {
        return (vec![base.clone()], false);
    }
    let total = bodies.len();
    let mut capped = false;
    if total > cap.max_parts {
        capped = true;
        bodies.truncate(cap.max_parts);
        let where_ = pointer.unwrap_or_else(|| save("report.txt", base.body.as_bytes()));
        let more = total - cap.max_parts;
        // the longest wording that still fits next to a little text; the path is never dropped
        let marker = [format!("\n[...truncated: {more} more part(s) not sent; full text: {where_}]"), format!("\n[+{more} parts: {where_}]"), format!("\n{where_}")]
            .into_iter()
            .find(|m| c.cost(&probe(m)) <= cap.limit)
            .unwrap_or_else(|| format!("\n{where_}"));
        if let Some(last) = bodies.last_mut() {
            while !last.is_empty() && c.cost(&probe(&format!("{last}{marker}"))) > cap.limit {
                last.pop();
            }
            last.push_str(&marker);
        }
    }
    let n = bodies.len();
    let mut out = Vec::with_capacity(n);
    for (i, body) in bodies.into_iter().enumerate() {
        let mut p = base.clone();
        if n > 1 {
            let label = format!("({}/{n})", i + 1);
            p.title = if base.title.is_empty() { label } else { format!("{} {label}", base.title) };
            p.id = format!("{}.{}", base.id, i + 1);
        }
        p.body = body;
        if i + 1 < n {
            p.link = None;
        }
        out.push(p);
    }
    (out, capped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::{Capacity, Ctx, FileCaps};
    use crate::http::SendError;

    /// cost = characters of title + body, like a plain-text channel.
    struct Plain {
        limit: usize,
        max_parts: usize,
        files: Option<FileCaps>,
    }
    impl Channel for Plain {
        fn kind(&self) -> &'static str {
            "plain"
        }
        fn describe(&self, _: &Message) -> String {
            String::new()
        }
        fn deliver(&self, _: &Message, _: &Ctx) -> Result<(), SendError> {
            Ok(())
        }
        fn secrets(&self) -> Vec<String> {
            vec![]
        }
        fn capacity(&self) -> Option<Capacity> {
            Some(Capacity { limit: self.limit, max_parts: self.max_parts })
        }
        fn cost(&self, m: &Message) -> usize {
            m.title.chars().count() + m.body.chars().count() + 1
        }
        fn files(&self) -> Option<FileCaps> {
            self.files
        }
    }

    fn saver(name: &str, data: &[u8]) -> String {
        format!("/saved/{name}.{}", data.len())
    }

    #[test]
    fn short_message_is_one_part() {
        let c = Plain { limit: 100, max_parts: 5, files: None };
        let p = plan(&c, &Message::new("t").body("short"), &saver);
        assert_eq!((p.parts.len(), p.how, p.text_parts), (1, "message", 1));
        assert_eq!(p.parts[0].title, "t");
    }

    #[test]
    fn empty_body_is_one_part() {
        let c = Plain { limit: 10, max_parts: 5, files: None };
        let p = plan(&c, &Message::new("t"), &saver);
        assert_eq!((p.parts.len(), p.how), (1, "message"));
    }

    #[test]
    fn splits_on_line_boundaries_and_numbers_the_parts() {
        let c = Plain { limit: 60, max_parts: 10, files: None };
        let body: String = (0..12).map(|i| format!("| row {i:02} | value |\n")).collect();
        let p = plan(&c, &Message::new("Audit").body(body.clone()), &saver);
        assert!(p.parts.len() > 1);
        assert_eq!(p.how, "split");
        let n = p.parts.len();
        for (i, m) in p.parts.iter().enumerate() {
            assert_eq!(m.title, format!("Audit ({}/{n})", i + 1));
            assert!(c.cost(m) <= 60, "{}", c.cost(m));
            for l in m.body.lines() {
                assert!(l.starts_with("| row ") && l.ends_with("| value |"), "table rows are never cut: {l}");
            }
        }
        let joined: String = p.parts.iter().map(|m| m.body.clone()).collect::<Vec<_>>().join("\n");
        assert_eq!(joined.trim_end(), body.trim_end(), "nothing lost, order kept");
    }

    #[test]
    fn a_long_single_line_is_cut_by_characters_without_loss() {
        let c = Plain { limit: 500, max_parts: 20, files: None };
        let line = "x".repeat(1200);
        let p = plan(&c, &Message::new("t").body(line.clone()), &saver);
        assert_eq!(p.how, "split");
        let joined: String = p.parts.iter().map(|m| m.body.as_str()).collect();
        assert_eq!(joined, line);
        assert!(p.parts.iter().all(|m| c.cost(m) <= 500));
    }

    #[test]
    fn polish_text_is_never_cut_in_half() {
        let c = Plain { limit: 41, max_parts: 50, files: None };
        let line = "zażółć gęślą jaźń 🙂 ".repeat(30);
        let p = plan(&c, &Message::new("t").body(line.clone()), &saver);
        let joined: String = p.parts.iter().map(|m| m.body.as_str()).collect();
        assert_eq!(joined, line.trim_end_matches('\n'));
        assert!(p.parts.iter().all(|m| c.cost(m) <= 41));
    }

    #[test]
    fn parts_beyond_the_cap_are_cut_with_a_visible_pointer() {
        let c = Plain { limit: 160, max_parts: 2, files: None };
        let body: String = (0..60).map(|i| format!("line number {i:02}\n")).collect();
        let p = plan(&c, &Message::new("t").body(body), &saver);
        assert_eq!((p.parts.len(), p.how), (2, "truncated"));
        let last = &p.parts[1].body;
        assert!(last.contains("[...truncated:") && last.contains("more part(s) not sent") && last.contains("/saved/report.txt"), "{last}");
        assert!(c.cost(&p.parts[1]) <= 160);
        assert_eq!(p.parts[1].title, "t (2/2)");
    }

    #[test]
    fn single_part_cap_is_not_numbered() {
        let c = Plain { limit: 150, max_parts: 1, files: None };
        let p = plan(&c, &Message::new("t").body("word ".repeat(100)), &saver);
        assert_eq!((p.parts.len(), p.how), (1, "truncated"));
        assert_eq!(p.parts[0].title, "t");
        assert!(p.parts[0].body.contains("[...truncated:"));
    }

    #[test]
    fn link_goes_on_the_last_part_only() {
        let c = Plain { limit: 60, max_parts: 10, files: None };
        let p = plan(&c, &Message::new("t").body("a line\n".repeat(30)).link("http://x"), &saver);
        let n = p.parts.len();
        assert!(p.parts[..n - 1].iter().all(|m| m.link.is_none()) && p.parts[n - 1].link.is_some());
    }

    #[test]
    fn file_travels_with_the_text_or_on_its_own() {
        let att = Attachment::new("raport.md", b"# report".to_vec());
        let with = Plain { limit: 100, max_parts: 5, files: Some(FileCaps { max_bytes: 1000, with_text: true }) };
        let p = plan(&with, &Message::new("t").body("summary").attach(att.clone()), &saver);
        assert_eq!((p.parts.len(), p.how), (1, "attachment"));
        assert_eq!(p.parts[0].attachments.len(), 1);
        let alone = Plain { limit: 100, max_parts: 5, files: Some(FileCaps { max_bytes: 1000, with_text: false }) };
        let p = plan(&alone, &Message::new("t").body("summary").attach(att), &saver);
        assert_eq!((p.parts.len(), p.text_parts, p.how), (2, 1, "attachment"));
        assert!(p.parts[0].attachments.is_empty() && p.parts[1].attachments.len() == 1 && p.parts[1].id.ends_with(FILE_PART));
        // a stored file part is not planned again
        let again = plan(&alone, &p.parts[1], &saver);
        assert_eq!((again.parts.len(), again.text_parts), (1, 0));
    }

    #[test]
    fn channel_without_files_gets_summary_and_pointer() {
        let none = Plain { limit: 200, max_parts: 5, files: None };
        let att = Attachment::new("raport.md", b"# report".to_vec());
        let p = plan(&none, &Message::new("t").body("summary").attach(att), &saver);
        assert_eq!((p.parts.len(), p.how), (1, "link"));
        assert_eq!(p.parts[0].body, "summary\n\nfull report: /saved/raport.md.8");
        assert!(p.parts[0].attachments.is_empty());
    }

    #[test]
    fn file_over_the_channel_limit_falls_back_to_the_pointer() {
        let small = Plain { limit: 200, max_parts: 5, files: Some(FileCaps { max_bytes: 4, with_text: true }) };
        let mut a = Attachment::new("r.md", b"0123456789".to_vec());
        a.path = Some("/home/me/r.md".into());
        let p = plan(&small, &Message::new("t").attach(a), &saver);
        assert_eq!(p.how, "link");
        assert_eq!(p.parts[0].body, "full report: /home/me/r.md", "a file that is already on disk is not copied");
    }
}
