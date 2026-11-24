# tests

Integration tests against local mock servers (std `TcpListener`) and the built binary. No real service is contacted.
Scratch files go to `$TMPDIR`; point it at a disk (`/tmp` may be RAM).

| file | covers |
|------|--------|
| `channels.rs` | every channel: exact path, headers, body, masking, 429, timeout, refused, unicode, big bodies |
| `reports.rs` | attachments and splitting per channel, partial failure, outbox round trip, limits |
| `flood.rs` | grouping, state changes, digest, flood guard, clock jumps, damaged state, concurrent processes, outbox atomicity |
| `serve.rs` | `notts serve`: socket mode, TCP token, concurrency, queue full, rate, oversize, shutdown |
| `cli.rs` | the frozen contract: version, golden JSON, exit codes, body and attach flags, masking |
| `clients.rs` | the snippets in `examples/` (Python, Go, sh, Node) against the built binary |
| `common/` | mock HTTP and SMTP servers, temp dirs, secret-file helper |
