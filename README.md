# notts

One message, every channel you enabled. notts carries a notification (title, body, severity, optional link,
optional file) to your phone, chat, mail or SMS gateway, with flood control so a noisy service cannot
bury the rest. It is a Rust library and a CLI (`notts`), plus an optional local server (`notts serve`).

It is the transport. It draws nothing on screen; the desktop notification daemon does that
(see "finedesktop hookup").

Status: version 0.1.0, contract 1. **Verified against local mock servers only, see "What is verified".**

## Channels

| channel   | route                                              | secret (never in the config)           | files                            |
|-----------|----------------------------------------------------|----------------------------------------|----------------------------------|
| ntfy      | HTTP PUT to a topic (recommended phone path)       | `topic_file=` / `topic_env=`           | `Filename` header, body = file   |
| telegram  | Bot API `sendMessage`, `sendDocument`              | `token_file=` / `token_env=`           | sendDocument (multipart)         |
| discord   | webhook, `@everyone`/`@here` stripped              | `webhook_file=` / `webhook_env=`       | one multipart request            |
| matrix    | client API: text event, media upload + `m.file`    | `token_file=` / `token_env=`           | upload then `m.file` event       |
| email     | SMTP (plain, STARTTLS or TLS), AUTH optional       | `pass_file=` / `pass_env=`             | multipart/mixed attachment       |
| webhook   | any HTTP endpoint, body template                   | `url_file=` / `url_env=`               | JSON `attachments`, text inline  |
| sms       | generic HTTP gateway adapter                       | `url_file=`, `to_file=` (or `_env`)    | no: summary + "full report: path"|
| desktop   | `notify-send` (optional local sink)                | none                                   | no: summary + path               |

SMS: notts speaks to an HTTP gateway you configure (a template for the request). It needs a paid or self-hosted
gateway; it was verified only against a mock server. There is no built-in carrier.

Secrets: a `*_file=` path must be a regular file with mode 600; a group- or world-readable file is refused with a clear
message. A channel whose secret is missing or unreadable is `not-configured`, never `sent`. Tokens, URLs that carry
tokens and phone numbers are masked in every error, detail, log line and test output.

## Install and build

    cargo build --release          # target/release/notts
    ./check                        # fmt --check, clippy -D warnings, tests (offline cargo cache)

Dependencies: ureq (rustls), serde_json, libc, rustls, webpki-roots. No async runtime. A static musl build was not
tried (no musl target installed here).

## Config

One plain-text file, `directive args key=value`, default `$XDG_CONFIG_HOME/notts/notts.conf`
(or `--config FILE`, or `$NOTTS_CONFIG`). See `notts.conf.example` for a full file with `judged`,
`smart-optimizer` and `finedesktop` as three app names.

    set timeout=10 retries=2 backoff=1000 group=10m
    channel phone type=ntfy server=https://ntfy.sh topic_file=~/.secrets/notts-ntfy-topic
    channel telegram token_file=~/.secrets/notts-telegram-token chat=123456789
    rule app=judged min=fail to=phone,telegram
    rule app=judged min=warn to=telegram
    digest at=08:00,18:00 severities=info,warn lines=30 to=mail
    flood max=30 cooldown=10m
    quiet 22:00-07:00 except=fail

Directives: `set`, `channel NAME [type=KIND] key=value...`, `rule app= tag= min= to=` (globs on app and tag;
a message goes to the union of the matching rules), `quiet HH:MM-HH:MM except=fail` (across midnight and `24:00`
work), `digest every=30m | at=HH:MM,... severities= lines= to=`, `flood max= cooldown= | off`,
`required=yes` on a channel (exit 1 if it is not delivered). A CRLF file and a BOM are accepted. A bad line is an
error naming the line number (exit 2).

## CLI (contract 1, frozen)

    notts [--config FILE] [--state-dir DIR] send --title T [--body B | --body-file FILE|- | stdin]
          [--app NAME] [--severity ok|warn|fail|info] [--link URL] [--tag T] [--key K]
          [--state ok|warn|fail] [--to ch1,ch2] [--dry-run] [--no-group] [--json]
          [--attach FILE [--attach-name NAME]] ... (up to 3, each name pairs with the attachment before it)
    notts flush                       retry the outbox
    notts status [--json]             channels (configured, or why not) and the outbox size
    notts log [--app A] [--since 2h] [--grouped]
    notts digest [--now] [--dry-run] [--json]
    notts hook                        one message from finedesktop's FD_* environment
    notts watch                       forward desktop notifications (needs busctl)
    notts serve [--socket PATH] [--no-socket] [--listen 127.0.0.1:PORT --token-file FILE]
                [--queue N] [--workers N] [--rate PER_MIN] [--max-body BYTES]
    notts --version                   "notts 0.1.0 (contract 1)"

`--app NAME` is the routing key: rules match on it. Stdin longer than 4 MiB is an error (exit 2), never a cut.

Exit codes: **0** delivered to at least one channel, or deliberately not sent (grouped, held, suppressed);
**1** nothing delivered, or a `required=yes` channel failed; **2** bad usage or config; **3** queued in the
outbox only (retry with `notts flush`).

`--json` prints one object. The field order and names are pinned by a golden test:

    {"notts_version":"0.1.0","contract":1,"exit_code":0,"disposition":"sent","reason":"sent; hook=sent",
     "key":"Disk almost full",
     "results":[{"name":"hook","status":"sent","detail":"delivered on attempt 1","delivered_as":"message","parts":1}]}

- `disposition`: what happened to the message: `sent`, `grouped`, `held`, `suppressed`, `queued`.
- per channel `status`: `sent`, `failed`, `skipped`, `not-configured`, `deduped`, `queued`;
  `detail` never contains a secret.
- `delivered_as`: `message`, `split`, `attachment`, `link`, `truncated`; `parts` is the number of parts.

New fields may be added at the end in a minor version; names and meanings of existing ones change only with
`contract` 2.

## Flood control

Same config, tested with a fake clock. Repeats of one key (default app + title, or `--key`) inside `group=` are
counted, not sent; the next message carries the count ("Disk almost full (x7 since 14:02)").
`--state ok|warn|fail --key K` notifies only when the state changes. Digests collect what is below fail as ONE
message per period (worst first, "and N more", never empty). `fail` is never held except by quiet hours with no
`except=fail`. An app over `flood max=` per minute is switched to the digest for the cooldown and you get one
notice. Rules decide which channel hears what (phone: fail and resolved; chat: also warn; mail: the digest).
Every decision is one line in the history: `notts log`. `notts digest --now` sends the pending digest.
The model is in `DESIGN.md`.

## Whole reports: attachments and long bodies

    notts send --app judged --key judged-audit --state fail --title "Audit: 3 findings" \
         --body-file summary.txt --attach raport.md --attach-name audit-2026-11-24.md

`--body-file -` reads stdin. Per channel the best native route is used (table above) and reported as
`delivered_as`. A channel that cannot carry files gets the summary plus `full report: <path>` with a saved copy.
A long body without a file is split on line boundaries at the channel's real limit (Telegram 4096, Discord 2000,
Matrix capped, SMS 160 per segment, 3 segments by default), numbered `(1/3)`, in order. Anything beyond the cap is
marked `[...truncated: N more part(s) not sent; full text: PATH]`; nothing is cut silently. The
state key applies to the whole report: an unchanged failing audit is not re-sent (history says
`suppressed: unchanged`), ok -> fail sends summary and file, fail -> ok sends "resolved" without the file.
Attachment contents are never logged, only name, size and sha256.

## Using notts from ...

The exact snippets below live in `examples/` and are run by `crates/notts/tests/clients.rs`.

**Shell**

    printf '%s' "$body" | notts send --app smart-optimizer --severity info --title "Cache cleaned" --json

**Python** (`examples/send.py`)

    r = subprocess.run(["notts", "send", "--app", "judged", "--severity", "warn", "--title", title, "--json"],
                       input=body.encode(), capture_output=True, timeout=300)
    code, report = r.returncode, json.loads(r.stdout or b"{}")

**Go** (`examples/send.go`)

    cmd := exec.Command("notts", "send", "--app", app, "--severity", sev, "--title", title, "--json")
    cmd.Stdin = strings.NewReader(body)
    out, err := cmd.Output() // exit code 1 or 3 still returns the JSON on stdout; check *exec.ExitError

**Rust** (library, no process). Path dependency or git dependency:

    notts = { path = "../notts/crates/notts" }
    notts = { git = "https://github.com/OWNER/notts", package = "notts" }   # once the repo is published

    let n = notts::Notts::from_config(&notts::default_config_path())?;
    let report = n.send(&notts::Message::new("Disk almost full").severity(notts::Severity::Warn).app("judged"),
                        &notts::SendOptions::new());
    // report.exit_code, report.disposition, report.results: Vec<ChannelResult>

The API is `Message`, `Severity`, `Notts`, `SendOptions`, `Report`, `ChannelResult`, `Status`, `Disposition`, `Init`,
`Config`, `Clock`. It never panics on bad input and keeps no global state; the rest of the crate is internal.

## notts serve (for long-running services)

    notts serve --socket "$XDG_RUNTIME_DIR/notts.sock"              # mode 600, the default location
    notts serve --listen 127.0.0.1:7780 --token-file ~/.config/notts/serve.token   # optional, loopback only

- `POST /v1/send` takes the same fields as the CLI as JSON (`app severity title body link tag key state to dry_run
  no_group attachments`) and returns the CLI's `--json` object. 200 delivered, 202 queued or held, 502 nothing delivered.
  `attachments` is a list of `{"path": ...}` (unix socket only) or `{"name": ..., "base64": ...}`; at most 3, 8 MiB each.
- `GET /v1/health`, `GET /v1/outbox` (count only).
- Back-pressure: a full queue answers 429 with `Retry-After`; there is a per-app rate limit and a request size cap
  (413). A bad or missing bearer token is 401. A non-loopback `--listen` address is refused. Shutdown flushes
  queued messages to the outbox. The same dedupe, quiet hours, rules and retries as the CLI apply.

Go over the unix socket (`examples/serve_client.go`):

    c := &http.Client{Transport: &http.Transport{DialContext: func(ctx context.Context, _, _ string) (net.Conn, error) {
        return (&net.Dialer{}).DialContext(ctx, "unix", sock) }}}
    resp, err := c.Post("http://notts/v1/send", "application/json", strings.NewReader(`{"app":"fastkit-api","severity":"warn","title":"Queue stuck"}`))

Node / TypeScript (`examples/serve_client.mjs`):

    const req = http.request({ socketPath: sock, path: "/v1/send", method: "POST",
                               headers: { "content-type": "application/json" } }, (res) => { /* read res */ });
    req.end(JSON.stringify({ app: "fastkit-web", severity: "warn", title: "Build failed" }));

A systemd user unit is in `examples/notts.service`.

## finedesktop hookup

finedesktop-rs has the on-screen daemon and a hook `on-notify` that runs for every notification with
`FD_TEXT`, `FD_APP`, `FD_URGENCY`, `FD_TAG` set. The simplest real hookup, with no change to finedesktop-rs, is an
executable `~/.config/finedesktop/hooks/on-notify`:

    #!/bin/sh
    exec notts hook

`notts hook` turns that environment into one message (app defaults to `finedesktop`, urgency becomes severity) and
ignores notifications from app `notts`, so the desktop sink cannot loop. Without finedesktop,
`notts watch` forwards any `org.freedesktop.Notifications` traffic through `busctl --user monitor`.
Do not enable the `desktop` channel for the same messages the hook forwards: you would see each twice.

TODO (not done): a native call from nottsd instead of a script; per-app routing keyed on `FD_APPID`;
a documented urgency-to-severity table.

## judged adapter (not applied)

`judged/judged/notify.py` sends through `apprise`. The one-line change to route it through notts (judged is not
modified by this repo): replace the `apprise` call with

    subprocess.run(["notts", "send", "--app", "judged", "--severity", sev, "--title", title, "--key", key,
                    "--state", state, "--body-file", "-", "--json"], input=body.encode(), timeout=300)

and add `--attach report.md` for a whole report. Exit code 3 means queued: not an error.

## What is verified

Verified against mocks only: every channel (ntfy, Telegram, Discord, Matrix, webhook, SMS gateway, e-mail via a mock
SMTP server) was exercised only against local `TcpListener` mock servers that record the exact path, headers and body;
nothing real was contacted, no credentials were used. The tests check secret masking, 429 and `Retry-After`, timeouts,
connection refused, retry with a fake clock, invalid UTF-8, a 1 MB body, empty body, splitting, attachments, the outbox
(kill between write and rename), dedupe, digest, flood guard, state changes, a clock that jumps back, two and eight
concurrent processes on one state directory, `notts serve` with concurrent clients, queue-full, a bad token, an
oversized body and socket permissions, and the Python, Go, shell and Node snippets.

One real delivery was made: a clearly labelled test message to the local D-Bus session through the `desktop`
channel (notify-send to the running notification daemon).

Not verified: any real server's behaviour (real Telegram, Discord, Matrix, ntfy, SMTP and TLS interoperability),
ntfy attachment limits on a real server, a real SMS gateway, the static musl build, the systemd unit, `notts watch`
against a live bus, the finedesktop `on-notify` hook run by the real desktop. Real servers may differ from the
mocks, which follow the public API documentation as the author understood it.

## Layout

    crates/notts/src        library and CLI (README inside)
    crates/notts/tests      mock-server and process tests (README inside)
    examples                the snippets above, run by the tests
    notts.conf.example      full example config
    check                   fmt, clippy, tests
    mutate                  break one line on purpose, run the tests, restore (mutation check)

MIT licensed. Design notes and rejected alternatives: `DESIGN.md`.
