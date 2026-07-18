# Reading the macrdp audit log

macrdp emits a security **audit event** for each connection lifecycle step — who
connected, whether the guard let them in, whether they authenticated, and how the
session ended. This page explains **what each event and field means and how to
interpret them**. For getting these events off-box into a SIEM/SOC (collector
configs, JSON stream), see [`siem-forwarding.md`](siem-forwarding.md); for the
knobs, see [`configuration.md`](configuration.md).

## Where the events are, and in what format

Every audit event is a `tracing` record on the dedicated **`macrdp::audit`**
target, written to up to two places:

- **Human-readable** — always in the main log, `~/Library/Logs/macrdp.log`
  (logfmt: `key=value` pairs). Find them with:
  ```bash
  grep 'macrdp::audit' ~/Library/Logs/macrdp.log
  ```
- **Structured JSON** — one JSON object per line, in a dedicated self-rotating
  file, **only when you opt in** with `--audit-file PATH` / `AUDIT_FILE` /
  `MACRDP_AUDIT_JSON=1`. This is the machine-parse target.

> **Parse the JSON stream, not the logfmt line.** The human-readable line's field
> order and spacing come from the `tracing` formatter and are not a stable
> contract; the JSON stream is versioned (`schema_version`) and is what tooling
> should consume.

The same event is written to both sinks, e.g. an accepted connection:

```text
# logfmt (macrdp.log)
2026-07-10T18:22:04.117Z  INFO macrdp::audit: schema_version=1 macrdp_version="0.8.32" host="mac-studio" event="accept" src_ip=203.0.113.5 src_port=54132
```
```json
// JSON (--audit-file)
{"timestamp":"2026-07-10T18:22:04.117Z","level":"INFO","target":"macrdp::audit","schema_version":1,"macrdp_version":"0.8.32","host":"mac-studio","event":"accept","src_ip":"203.0.113.5","src_port":54132}
```

## The five events

macrdp maps one TCP connection to (usually) four events in order —
**`accept` → `auth` → `fingerprint` → `disconnect`** — plus **`reject`** for
connections the guard blocks *before* they ever handshake.

### `accept` — the guard let the connection through *(INFO)*
Emitted the moment a connection passes the pre-handshake auth guard (per-IP
rate-limit + lockout checks) and is handed to the TLS/CredSSP stack. **It does
not mean the client authenticated** — only that it was allowed to try. Every
accepted connection should be followed by an `auth` and a `disconnect` with the
same `(src_ip, src_port)`.

### `reject` — the guard blocked it before any handshake *(WARN)*
Emitted when the per-IP guard refuses the connection outright; it is dropped with
no TLS, no CredSSP, no `accept`. `reason` says why:

- **`reason="rate_limit"`** — too many connection attempts from this IP inside the
  sliding window (default 10 / 60 s). `window_attempts` is how many were counted.
- **`reason="lockout"`** — this IP is in an escalating cooldown after repeated
  fast failures (default: after 5 consecutive, 30 s doubling to a 15 min cap).
  `retry_after_secs` is how long until the cooldown expires.

A `reject` carries **`src_ip` but not `src_port`** (the decision is per-IP, made
before the port matters) and has **no matching `accept`** — correlate rejects by
`src_ip` + time, not by the connection tuple.

### `auth` — the CredSSP/NLA login verdict *(INFO on success, WARN on failure)*
Emitted **once per connection**, right after the TLS upgrade, when the credential
exchange resolves. This is the authoritative login result:

- **`outcome="success"`** — the client's credentials validated against the macrdp
  account. *(INFO)*
- **`outcome="did_not_complete"`** — authentication did not finish. Dominated by a
  **wrong username/password**, but also covers a client aborting the credential
  dialog or a rare mid-exchange transport error. `reason` is a short sspi error
  description (sanitized, ≤200 chars, never credential material) that
  disambiguates. *(WARN)*

Because it fires *after* the TLS upgrade, a benign pre-TLS blip (e.g. mstsc's
first-connect certificate-trust prompt reopening the socket) happens **before**
this point and can never produce a false `auth` failure — such a connection simply
has an `accept` and a `disconnect` with **no `auth` event in between**, which is
itself the tell that it never reached authentication.

> **Scope:** the `auth` event is emitted on the single-process server path. See
> `configuration.md`.

### `fingerprint` — which RDP client connected *(INFO)*
Emitted **once per connection** when the capability exchange completes (after
`auth`; not re-emitted on an in-session reactivation such as a live resize or
blank recovery). Carries the identity the client announced during the handshake:

- **`client_name`** — the client machine's hostname (client-controlled;
  sanitized: control-chars stripped, length-bounded).
- **`rdp_version`** — the announced RDP protocol version, hex (e.g. `0x80011` =
  RDP 10.12).
- **`client_build`** — the client's announced build number.
- **`platform`** — the OS platform from the client's General capability set.

**This is fingerprinting, not authentication** — a client can claim anything.
Live-verified signatures for telling clients apart:

| Client | `client_build` | `platform` |
|---|---|---|
| **mstsc** (real Windows) | the actual Windows build (e.g. `26100` = Win11 24H2, `22621` = 22H2) | `WINDOWS/WINDOWS_NT` |
| **Thincast** | `18363` (a fixed, claimed value — not the host's real build) | `UNSPECIFIED/UNSPECIFIED` |
| **FreeRDP** family | `2600` (hardcoded XP build) | `UNIX/...` |
| **Windows App** (macOS/iOS/Android) | varies | its host platform |

Reading it: mstsc reports the *machine's real* build and a Windows platform;
everyone else reports a fixed/claimed build and (for the FreeRDP family, which
Thincast derives from) a non-Windows or unspecified platform. Use it for "which
client is this?" triage — never as a trust signal.

> **Scope:** the `fingerprint` event is emitted on the single-process server
> path (same as `auth`).

### `disconnect` — the connection ended *(INFO)*
Emitted when the connection closes, with `duration_ms` (wall-clock lifetime) and a
heuristic `outcome`:

- **`outcome="success"`** — a clean session, **or** any connection that got past
  the handshake (treated as legitimate; it resets the IP's failure counter).
- **`outcome="failure"`** — the connection errored **and** failed fast (within the
  ~3 s fail-fast window) — the brute-force/scan signature. Only this classified
  failure accrues toward a lockout.

> **`disconnect.outcome` is a heuristic for the lockout logic, not the login
> verdict.** For "did this login succeed?", read the **`auth`** event.
> `disconnect.outcome="failure"` means "errored + fast" (looked like a scan);
> `auth.outcome="did_not_complete"` means "authentication actually failed." They
> usually agree, but a long benign session that errors late (e.g. a flaky link)
> is `disconnect.outcome="success"` with no `auth` failure.

## Field reference

| field | type | on events | meaning |
|---|---|---|---|
| `timestamp` | string | all | RFC3339 UTC when the event was recorded (JSON only; logfmt shows it as the line prefix) |
| `level` | string | all | `INFO`, or `WARN` for `reject` and `auth` failure |
| `target` | string | all | always `macrdp::audit` (the grep key) |
| `schema_version` | int | all | audit contract version (`1`); bumps only on a breaking field change |
| `macrdp_version` | string | all | server build, e.g. `0.8.32` |
| `host` | string | all | server hostname (a collector usually adds its own too) |
| `event` | string | all | `accept` \| `reject` \| `auth` \| `fingerprint` \| `disconnect` |
| `src_ip` | string | all | client source IP — the primary correlation key |
| `src_port` | int | accept, auth, fingerprint, disconnect | client source port — completes the per-connection tuple. **Absent on `reject`.** |
| `reason` | string | reject, auth (failure only) | reject: `rate_limit` \| `lockout`. auth: sanitized sspi error text |
| `window_attempts` | int | reject (`rate_limit`) | attempts counted in the current window |
| `retry_after_secs` | int | reject (`lockout`) | seconds until the cooldown expires |
| `outcome` | string | auth, disconnect | auth: `success` \| `did_not_complete`. disconnect: `success` \| `failure` |
| `client_name` | string | fingerprint | client's announced hostname (client-controlled; sanitized) |
| `rdp_version` | string | fingerprint | announced RDP protocol version, hex |
| `client_build` | int | fingerprint | client's announced build number |
| `platform` | string | fingerprint | OS platform from the General capset (server-formatted) |
| `duration_ms` | int | disconnect | connection wall-clock lifetime in milliseconds |

**Correlation:** an `accept`, its `auth`, and its `disconnect` share
`(src_ip, src_port)`. Rejects have no port and no matching accept — group them by
`src_ip`. Ephemeral source ports get reused over time, so bound correlation by a
time window; a monotonic per-connection id is a possible future additive field.

## Reading common patterns

**Normal successful session**
```text
event="accept"      src_ip=203.0.113.5 src_port=54132
event="auth"        src_ip=203.0.113.5 src_port=54132 outcome="success"
event="fingerprint" src_ip=203.0.113.5 src_port=54132 client_name="GENMACWIN" client_build=26100 platform="WINDOWS/WINDOWS_NT"
event="disconnect"  src_ip=203.0.113.5 src_port=54132 duration_ms=216913 outcome="success"
```
Allowed → authenticated → identified as real mstsc → clean multi-minute session.
The baseline.

**A single wrong password**
```text
event="accept"     src_ip=203.0.113.9 src_port=51020
event="auth"       src_ip=203.0.113.9 src_port=51020 outcome="did_not_complete" reason="logon denied"  (WARN)
event="disconnect" src_ip=203.0.113.9 src_port=51020 duration_ms=850 outcome="failure"
```
One bad login: the `auth` WARN is the authoritative signal; the fast `failure`
disconnect is what the lockout counter watches.

**Brute force / password spray → lockout**
```text
… five (or more) accept → auth did_not_complete → disconnect failure cycles from 203.0.113.9 …
event="reject" reason="lockout" src_ip=203.0.113.9 retry_after_secs=30   (WARN)
event="reject" reason="lockout" src_ip=203.0.113.9 retry_after_secs=60   (WARN)   ← escalating
```
After the threshold, the guard stops handing the IP to the stack; each further
attempt is a `reject` with a doubling `retry_after_secs`. A burst faster than the
window instead shows `reason="rate_limit"` with `window_attempts`.

**Benign client blip (e.g. mstsc cert prompt)**
```text
event="accept"     src_ip=203.0.113.5 src_port=54120
event="disconnect" src_ip=203.0.113.5 src_port=54120 duration_ms=140 outcome="failure"
event="accept"     src_ip=203.0.113.5 src_port=54121
event="auth"       src_ip=203.0.113.5 src_port=54121 outcome="success"
event="disconnect" src_ip=203.0.113.5 src_port=54121 outcome="success"
```
The first connection ended **before** `auth` (no `auth` event) — it never reached
authentication, so it is not a failed login. A single such fast failure never
locks anyone out (the threshold is consecutive failures, and the next clean
session resets the counter).

**Loopback** — `127.0.0.1` / `::1` is exempt from the guard's enforcement but is
**still audited**, so you will see `accept`/`auth`/`disconnect` for local
connections (dev, `--skip-auth`). These are local, not remote logins; filter them
out or treat them as low signal in a SOC.

## Interpretation gotchas

- **No audit events at all?** The audit handler only exists when the connection
  guard is enabled. `MACRDP_CONN_GUARD=0` disables the whole subsystem — including
  audit — even with `MACRDP_AUDIT_LOG=1`. To keep audit while disabling
  *enforcement*, leave `MACRDP_CONN_GUARD` on and zero the thresholds
  (`MACRDP_GUARD_RL_MAX=0`, `MACRDP_GUARD_FAIL_THRESHOLD=0`).
- **`did_not_complete` ≠ always "wrong password."** It is dominated by bad
  credentials but the `reason` string is what tells logon-denied from a client
  abort or transport reset. Do not alert solely on the count without reading it.
- **Volume is bounded.** Rejected connections never reach the stack, so a
  brute-forcer's accepted attempts are capped by the lockout, and the audit file
  self-rotates — the stream can't run away under attack.
- **`macrdp_version` / `schema_version`** let you pin detection rules across
  upgrades; key alerts off `schema_version` so an additive field never breaks a
  parser.

## Verifying it locally

[`scripts/test-audit-log.sh`](../scripts/test-audit-log.sh) exercises the whole
path end-to-end with no real password and no GUI: it starts a loopback macrdp
(`--skip-auth` against a throwaway credential), drives one correct- and one
wrong-password `sdl-freerdp +auth-only` connection, and asserts the JSON audit
stream recorded `auth` `success` / `did_not_complete` (with a clean `reason`) plus
the `accept`/`disconnect` correlation. Needs `sdl-freerdp` (`brew install
freerdp`); exit 0 = pass.

## See also
- [`siem-forwarding.md`](siem-forwarding.md) — forwarding the JSON stream to a SIEM (Vector / Fluent Bit / rsyslog).
- [`siem-tutorial.md`](siem-tutorial.md) — a runnable end-to-end walkthrough: OpenSearch SIEM on your Mac detecting an RDP brute-force.
- [`configuration.md`](configuration.md) — `--audit-file`, `MACRDP_AUDIT_*`, and the connection-guard thresholds.
