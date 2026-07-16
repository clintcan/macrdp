# SIEM / SOC forwarding — the JSON audit stream

macrdp emits its security-relevant events (connection **accept / reject / auth / disconnect**,
with source IP+port, reason, and outcome) on a dedicated `macrdp::audit` tracing target. Point
`--audit-file` at a file and those events are additionally written as **one JSON object per
line** — a stable, versioned contract a log collector can tail and forward to a SIEM.

> For what each event and field **means** and how to read them (patterns for a
> normal session, a wrong password, a brute-force lockout, etc.), see the
> companion [`audit-log.md`](audit-log.md). This page is about getting the stream
> off-box.
>
> Want a runnable end-to-end example? [`siem-tutorial.md`](siem-tutorial.md) stands up a real
> open-source SIEM (OpenSearch) on your Mac and detects an RDP brute-force against macrdp,
> copy-paste, in ~15 minutes.

macrdp deliberately does **not** speak network syslog itself. Getting logs off-box reliably
(TLS, buffering, reconnect, backpressure) is a solved problem owned by collector agents; a
software RDP server should not re-implement it on its hot path. And on macOS there is no
first-class network-syslog daemon anyway — unified logging replaced `syslogd` — so the
supported integration is: **macrdp → JSON file → collector agent → SIEM.**

## Enabling it

Off by default. Enable either way (both opt-in; the normal runtime path is byte-identical
when off):

```bash
# Explicit path (CLI, config.env AUDIT_FILE=, or LaunchAgent EnvironmentVariables):
macrdp --audit-file ~/Library/Logs/macrdp-audit.log …

# Or flip the switch to use the default <log-dir>/macrdp-audit.log:
MACRDP_AUDIT_JSON=1 macrdp …
```

Knobs:

| knob | default | meaning |
|---|---|---|
| `--audit-file PATH` / `AUDIT_FILE` | (off) | write JSON audit lines to PATH |
| `MACRDP_AUDIT_JSON=1` | off | enable at the default `<log-dir-or-~/Library/Logs>/macrdp-audit.log` |
| `MACRDP_AUDIT_LOG` / `AUDIT_LOG` | on | gates whether audit events emit **at all** (both sinks) |
| `MACRDP_AUDIT_LOG_MAX_BYTES` | 10 MiB | audit-file rotation size |
| `MACRDP_AUDIT_LOG_MAX_FILES` | 5 | audit-file archive count |

Notes:
- The human-readable audit lines still appear in `macrdp.log` (logfmt) unchanged — the JSON
  file is an **additional** sink, not a move.
- The JSON stream is emitted **independent of `RUST_LOG`** (a per-layer filter pins the
  `macrdp::audit` target at `INFO`), so raising operational verbosity to `warn` never
  suppresses security events.
- The file self-rotates (stable live name `macrdp-audit.log`, archives `.1`, `.2`, …), so the
  collector always tails the same path.
- **Loopback is exempt** from the auth guard, and audit events fire for real (non-loopback)
  peers — i.e. the stream is meaningful only when `--bind` exposes the server off-loopback.

## Schema (v1)

One JSON object per line. `schema_version` pins the contract — it bumps only on a **breaking**
field change (rename/removal/semantic shift), never for additive fields, so detection/parse
rules can key off it safely.

Fields common to the events (all carry these except where the `src_port` row notes otherwise):

| field | type | notes |
|---|---|---|
| `timestamp` | string | RFC3339 UTC |
| `level` | string | `INFO` (accept/disconnect, auth success) or `WARN` (reject, auth failure) |
| `target` | string | always `macrdp::audit` |
| `schema_version` | int | `1` |
| `macrdp_version` | string | e.g. `0.8.32` |
| `host` | string | server hostname (a collector usually adds its own too) |
| `event` | string | `accept` \| `reject` \| `auth` \| `disconnect` |
| `src_ip` | string | peer IP — correlation key |
| `src_port` | int | peer source port — correlation key (present on `accept`/`auth`/`disconnect`; **absent on `reject`**, which is a per-IP decision) |

Event-specific:

| event | extra fields |
|---|---|
| `accept` | — |
| `reject` | `reason` (`rate_limit` \| `lockout`), and `window_attempts` **or** `retry_after_secs` |
| `auth` | `outcome` (`success` \| `did_not_complete`), and `reason` (only on `did_not_complete`) |
| `disconnect` | `duration_ms`, `outcome` (`success` \| `failure`) |

The `auth` event is the explicit CredSSP/NLA login verdict, emitted once when the exchange
resolves: `success` = the client's credentials validated; `did_not_complete` = auth did not
finish (dominated by a wrong password, but also a client abort or a rare mid-exchange transport
error — the `reason`, a short sspi error description, disambiguates and never contains
credential material; it is control-char-stripped and length-bounded, so it is always a safe
single-line token — no log-injection risk in either sink). It's strictly better than inferring the verdict from `disconnect`'s
duration heuristic. **v1 caveats:** it carries no client-*attempted* username (macrdp authenticates a
static credential — surfacing the attempted user is a possible future additive field).

**Correlation:** an `accept`, its `auth`, and its matching `disconnect` share the
`(src_ip, src_port)` tuple. (Ephemeral ports can be reused over time; a monotonic
per-connection id is a possible future additive field.)

### Example lines

```json
{"timestamp":"2026-07-10T18:22:04.117Z","level":"INFO","target":"macrdp::audit","schema_version":1,"macrdp_version":"0.8.32","host":"mac-studio","event":"accept","src_ip":"203.0.113.5","src_port":54132}
{"timestamp":"2026-07-10T18:22:04.402Z","level":"INFO","target":"macrdp::audit","schema_version":1,"macrdp_version":"0.8.32","host":"mac-studio","event":"auth","src_ip":"203.0.113.5","src_port":54132,"outcome":"success"}
{"timestamp":"2026-07-10T18:22:09.882Z","level":"WARN","target":"macrdp::audit","schema_version":1,"macrdp_version":"0.8.32","host":"mac-studio","event":"reject","reason":"lockout","src_ip":"203.0.113.9","retry_after_secs":240}
{"timestamp":"2026-07-10T18:25:41.030Z","level":"INFO","target":"macrdp::audit","schema_version":1,"macrdp_version":"0.8.32","host":"mac-studio","event":"disconnect","src_ip":"203.0.113.5","src_port":54132,"duration_ms":216913,"outcome":"success"}
```

## Forwarding with a collector

Any file-tailing agent works. Examples (fill in your SIEM's sink + TLS):

### Vector

```toml
[sources.macrdp_audit]
type = "file"
include = ["/Users/<you>/Library/Logs/macrdp-audit.log"]
read_from = "beginning"

[transforms.macrdp_parse]
type = "remap"
inputs = ["macrdp_audit"]
source = '. = parse_json!(.message)'

# Example sink: Splunk HEC (swap for elasticsearch / datadog_logs / loki / syslog).
[sinks.siem]
type = "splunk_hec_logs"
inputs = ["macrdp_parse"]
endpoint = "https://splunk.example.com:8088"
default_token = "${SPLUNK_HEC_TOKEN}"
[sinks.siem.tls]
verify_certificate = true
```

### Fluent Bit

```ini
[INPUT]
    Name        tail
    Path        /Users/<you>/Library/Logs/macrdp-audit.log
    Tag         macrdp.audit
    Parser      json
    Refresh_Interval 5

[OUTPUT]
    Name        splunk
    Match       macrdp.audit
    Host        splunk.example.com
    Port        8088
    Splunk_Token ${SPLUNK_HEC_TOKEN}
    TLS         On
```
(`parsers.conf`: a `[PARSER] Name json / Format json / Time_Key timestamp` entry.)

### rsyslog (for shops that specifically want syslog transport)

```
module(load="imfile")
input(type="imfile"
      File="/Users/<you>/Library/Logs/macrdp-audit.log"
      Tag="macrdp-audit"
      ruleset="macrdp_fwd")

ruleset(name="macrdp_fwd") {
    # $msg already IS the JSON object; forward as RFC5424 to the SIEM over TLS.
    action(type="omfwd" target="siem.example.com" port="6514" protocol="tcp"
           StreamDriver="gtls" StreamDriverMode="1" StreamDriverAuthMode="x509/name")
}
```

macOS note: rsyslog isn't shipped by Apple; install via Homebrew if you go this route.
Vector/Fluent Bit are the lower-friction options on a Mac.

## What a SOC gets today, and what could be added

Emitted now: connection accept, reject (rate-limit / lockout, with retry-after), the explicit
CredSSP/NLA **auth** verdict (success / did-not-complete + reason), and disconnect (duration +
success/failure outcome). That's the core RDP-server auth telemetry.

Natural follow-ons (each a one-line `tracing::info!(target: "macrdp::audit", …)` once the
schema+sink exist, additive — no `schema_version` bump): TLS cert load / expiry warning, the
health-watchdog bounce, which redirection features are active at startup (drive / smart-card /
USB — they widen the trust surface), and an explicit session-start event. A native
RFC5424/CEF-over-TLS emitter inside macrdp is a deliberately deferred option for deployments
that truly can't run a collector.
