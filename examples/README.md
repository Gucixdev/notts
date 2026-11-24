# examples

Minimal clients, run by `crates/notts/tests/clients.rs` against the built binary and a mock server.
Run them by hand with `notts` on `PATH`.

| file | what |
|------|------|
| `send.py` | Python: `subprocess`, body over stdin |
| `send.go` | Go: `exec.Command` (`go run send.go APP SEVERITY TITLE [BODY]`) |
| `send.sh` | shell: `printf body \| notts send ...` |
| `serve_client.go` | Go: `net/http` over the `notts serve` unix socket |
| `serve_client.mjs` | Node: `node:http` with `socketPath` |
| `notts.service` | systemd user unit for `notts serve` (not run on a real systemd) |
