# notts design

notts is the remote side of notification: it carries one message to the channels you enabled
(phone, chat, mail, SMS, webhook, desktop). It does not draw anything on screen; finedesktop's
own daemon does that and calls `notts hook` for the messages that should leave the machine.

## Decisions

- Two stable doors: the library (`Message`, `Notts::from_config`, `send` -> `Report`) and the CLI
  (`notts send`, frozen in the README as contract 1). `notts serve` is the CLI over a socket, same JSON.
- One plain-text config, `directive args key=value`. No JSON/TOML: a line can be grepped, commented and diffed.
- Secrets are only ever a path to a mode-600 file or an env var name. A group/world-readable file is refused.
  Every value that was read is registered in a masker; all errors, details, logs and test output pass through it.
  A channel whose secret is missing is `not-configured`, never `sent`.
- Blocking HTTP (ureq + rustls), one scoped thread per channel. No async runtime: a send is a handful of requests.
- Time is a trait (`Clock`); every window, backoff, quiet hour and digest slot is tested with a fake clock.
- State is plain files in `$XDG_STATE_HOME/notts` (outbox, state, history), written by temp file + fsync + rename,
  guarded by an advisory `flock` so concurrent processes agree.
- Nothing is silently lost: every decision (sent, grouped, held, suppressed, queued) is one history line.

## Flood model (15 lines)

1. A message has a group key: `--key`, else app + title. Within `group=` repeats are counted, not sent.
2. The next message that is sent carries the count: "Disk almost full (x7 since 14:02)".
3. `--state ok|warn|fail` with a key: notify only when the state changes; state is persisted atomically.
4. A damaged state file is moved aside and treated as unknown: the message is sent once, nothing crashes.
5. `digest every=30m | at=08:00,18:00 severities=...` collects what is below fail, per app, worst first,
   with a visible "and N more", and sends ONE message per period. An empty digest is never sent.
6. `fail` is never held for a digest and never grouped away from the first occurrence; only `quiet ... except=`
   can hold it (and the rule says so explicitly).
7. Flood guard: an app over `max` messages per minute is switched to the digest for `cooldown`;
   one notice "app X is flooding, N messages held" is sent. Other apps are not affected (per-app accounting).
8. Rules route per channel: phone gets fail and resolved, chat also warn, mail the digest.
9. Quiet hours (also across midnight) hold everything except the listed severities; held items go to the digest.
10. Decision order: flood accounting, state change, routing, digest/cooldown hold, grouping, quiet hours.
11. A send first runs a digest that is due, then the new message.
12. A clock that jumps backwards never extends a cooldown or window beyond its configured length.
13. History records the reason for every decision; `notts log [--grouped]` reads it back.
14. `--dry-run` runs the whole decision and plan and touches neither network nor state.
15. Counting is exact; 500 messages from 5 apps produce a bounded number of sends (tested).

## Long bodies and attachments

Per channel the best native route, reported as `delivered_as`: message, split, attachment, link, truncated.
Channels that can carry a file (Telegram sendDocument, Discord multipart, Matrix media upload + m.file,
ntfy `Filename` PUT, e-mail attachment) get the summary plus the file. The webhook carries the text inline.
Channels that cannot (SMS gateway, template webhook) get the summary plus "full report: <path>" with a local copy
(mode 600, pruned by age). Without an attachment, a long body is split on line boundaries at each channel's real
limit (Telegram 4096, Discord 2000, Matrix capped, SMS 160 per segment with a cap on segments), numbered (1/3).
Beyond the cap the end carries a visible "[...truncated: N more part(s) not sent; full text: PATH]". Nothing is cut
silently. Attachment contents are never logged, only name, size and sha256. A resolved state (`--state ok`) sends
no file.

## Rejected alternatives

- Apprise/ntfy-only: the Python dependency tree is large and the flood model needs the same process to own state.
- Async runtime (tokio): more code, bigger binary, no benefit for a few requests per message.
- JSON/TOML config: harder to grep and comment, tempts people to put secrets inline.
- A daemon for everything: the CLI works with no daemon; `serve` is optional, for services that cannot shell out.
- Re-implementing the on-screen notification daemon: it exists in finedesktop, notts only forwards.
- Sending the full report by chunking everything: a file is the right unit on Telegram, Discord, Matrix, ntfy, mail.

## Why Rust and not Go

1. One static-friendly binary with a small dependency tree either way; Go would be as easy to ship.
2. The owner's other tools (finedesktop, progit, lazyUI) are Rust: the library can be a path dependency, no FFI.
3. Pattern-heavy state machines (`Disposition`, `Status`) with exhaustive matches catch missing cases at compile time.
4. No GC and no runtime means a short-lived CLI starts in about a millisecond; matters because every script shells out.
5. Cost: slower compile and a heavier TLS stack than Go's `crypto/tls`; accepted.
