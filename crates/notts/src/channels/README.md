# channels

One file per channel. Each turns a `Message` into one delivery (an HTTP request, an SMTP session, a local command).

`mod.rs` has the `Channel` trait, option parsing (`Opts`: `*_file`, `*_env` secrets), the allowed option names per kind
(a typo is a config error with a line number), `Capacity` (text limit and part cap) and `FileCaps` (attachments).

| file | channel |
|------|---------|
| `ntfy.rs` | ntfy PUT, `Filename` header for files |
| `telegram.rs` | Bot API sendMessage and sendDocument, parse-mode escaping |
| `discord.rs` | webhook, multipart for files, mentions stripped |
| `matrix.rs` | text event, media upload, `m.file` event |
| `email.rs` | SMTP with STARTTLS or TLS, multipart attachment |
| `webhook.rs` | generic HTTP with a body template |
| `sms.rs` | generic SMS gateway adapter (mock-verified only) |
| `desktop.rs` | `notify-send` |
