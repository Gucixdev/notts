# notts source

Library (`lib.rs`) and CLI (`main.rs`). Everything but the names re-exported in `lib.rs` is internal.

| file | what it does |
|------|--------------|
| `engine.rs` | `Notts`: send, flush, status; runs the pipeline, one thread per channel, outbox on retryable failure |
| `policy.rs` | the decision: flood accounting, state change, routing, digest hold, grouping, quiet hours |
| `plan.rs` | turns a message into parts for one channel: split, attach, link, truncate |
| `digest.rs` | digest collection and due-time logic |
| `config.rs` | the plain-text config parser (line numbers in errors), rules, quiet hours |
| `message.rs` | `Message`, `Severity`, `Attachment`, body caps and normalising |
| `state.rs` | groups, states, pending digests, flood counters; atomic file |
| `outbox.rs` | append-only outbox, `flush`, dead letters |
| `history.rs` | one line per decision; `notts log` |
| `lock.rs` | advisory file lock for concurrent processes |
| `secret.rs` | secret files (mode 600 check), env secrets, the masker |
| `http.rs` | blocking HTTP, retry/backoff, 429 and `Retry-After` |
| `text.rs` | escaping, truncation by chars or UTF-16, base64 |
| `clock.rs` | `Clock`, `SystemClock`, `FakeClock` |
| `serve.rs` | `notts serve` (unix socket and loopback HTTP) |
| `watch.rs` | parse `org.freedesktop.Notifications` traffic for `notts watch` |
| `channels/` | one file per channel (README inside) |
