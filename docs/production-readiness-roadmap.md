# Production-readiness roadmap

> Scoped 2026-06-29. **Not started** — a planning doc to come back to. Companion to
> the "Production readiness" section in `README.md` (which describes the *current*
> state); this describes what would *raise* it.

## Framing — the ceiling, and the realistic target

macOS puts a **hard ceiling** on "production": there is no multi-user concurrent
interactive GUI session model like Windows Terminal Services — one interactive
desktop per user, period. So the realistic target is **not** "enterprise RDP server."
It is:

> A **reliable, secure, unattended single-session server** you can deploy for yourself
> or a small team over a **LAN or VPN** and trust to stay up.

That is reachable. Everything below moves toward it; the [NO-GOs](#the-honest-no-gos)
are scope limits, not gaps to close.

## Tier 1 — Security (lifts the "trusted-LAN only" caveat)

1. **Real TLS certificates — DONE (2026-06-30).** The operator can now supply a real
   CA / ACME / Let's Encrypt cert/key via `--cert`/`--key` (or `TLS_CERT`/`TLS_KEY` in
   config.env), so clients can verify the server's identity instead of relying on
   trust-on-first-use. When set, macrdp uses exactly those files and **never** silently
   falls back to self-signed (a missing/bad file is a hard error), and it warns at
   startup if the cert is expired / within 14 days. Self-signed in `~/Library/Application
   Support/macrdp` remains the zero-config default. (Dropping `cert.pem`/`key.pem` into
   the cert dir also still works.) Not done here: ACME auto-renewal (operator tooling's
   job — replace the file + restart) and hot reload (a cert change needs a restart).
2. **Auth hardening — DONE (2026-06-30).** In front of the NLA/CredSSP pre-auth gate,
   macrdp now does per-source-IP connection **rate-limiting** + escalating auto-expiring
   **failed-attempt lockout** + a greppable **auth audit log** (`macrdp::audit` lines:
   who connected, from where, accept/reject/disconnect + outcome). On by default with
   conservative tunable thresholds (env / `config.env`); **loopback is exempt** so you
   can't self-lock. Lives in `src/auth_guard.rs` (a pure, unit-tested decision core) wired
   through the existing `ConnectionHandler` seam (single-process) and the `--fork-workers`
   supervisor loop — **zero vendored divergence**. The lockout is deliberately **heuristic**
   (errored/very-short ⇒ failure; clean long session resets), so a benign disconnect never
   locks anyone out. Not done here: precise CredSSP-failure classification (would need a
   vendored signal — intentionally avoided).
3. **Document the posture honestly.** Even hardened, internet-facing RDP is a bad idea
   for *any* server — the production answer is "behind a VPN or an RD Gateway." Make that
   explicit alongside Tier 1.1.

## Tier 2 — Reliability / unattended operation

4. **A real multi-day soak.** *(highest confidence per hour.)* The biggest unknown for
   "leave it running" is leaks/drift over time. Known suspects: the *audio long-session
   drift* item, and documented SCStream / NFS-mount leaks on hard kill (`SIGKILL` skips
   `Drop`). Run a 48–72 h soak (idle + active, with reconnect cycles) and fix what it
   surfaces.
5. **Robust teardown + log rotation.** *(log rotation + startup reaper SHIPPED 2026-06-30.)*
   - **Log rotation — DONE.** `~/Library/Logs/macrdp.log` is now a self-owned, size-bounded
     rotating file (`src/logging.rs`: `macrdp.log` + N logrotate-style archives, default
     10 MiB × 5; tunable via `MACRDP_LOG_MAX_BYTES`/`MACRDP_LOG_MAX_FILES`). The plist no
     longer redirects stdout there (panics → a small `macrdp.err.log`; a panic hook also
     routes panics into `macrdp.log` so the GUI still detects crashes).
   - **Startup reaper — DONE.** The graceful `SIGTERM`/`SIGINT` path already unmounts + cleans;
     the remaining leak was `SIGKILL`/panic (uncatchable in-process). `src/reaper.rs` now sweeps
     a *dead* prior process's leftovers on the next start (stale NFS mounts +
     `$TMPDIR/macrdp-{rdpdr,paste,lazy-paste}-<pid>` dirs), dead-pid-gated so it's safe with
     another instance live. (SCStreams / virtual display / blanking were already process-scoped
     and auto-restore.)
   - **Still TODO:** a lightweight **health-check** that detects a hung-but-alive process and
     bounces it (LaunchAgent `KeepAlive` already restarts on outright crash, but not on a hang).
6. **Make `--fork-workers` the production default.** It's what fixes the mstsc
   reconnect-blank that bites real users; recommend (or default) it for unattended
   deployments. Note it's mutually exclusive with `--enable-udp-multitransport`.

## Tier 3 — Polish / nice-to-have

7. **Single-session multi-monitor** (client-side multi-display). Achievable but **blocked**
   on the git-pinned `ironrdp-acceptor`'s single-monitor `MonitorLayoutPdu`; scoped/paused
   (see the multi-virtual-monitor TODO + memory).
8. **Auto-update** (e.g. Sparkle) so deployed instances stay current.
9. **A status / metrics surface** (active connections, fps, bitrate, error counts) for
   monitoring.
10. **Upstream the vendored IronRDP forks.** Reduces the long-term maintenance / bus-factor
    risk that "solo v0 on vendored forks" carries. See `project_upstream_ironrdp_open_prs`.

## The honest NO-GOs

Don't chase these — they're scope limits, not bugs:

- **Multi-user concurrent sessions** — macOS architectural limit (one interactive GUI
  session per user).
- **Capturing DRM video / secure-input fields** — OS-enforced; can't and shouldn't be
  overridden.
- **An enterprise SLA / commercial support** — it's a one-person project.

## Recommendation — the "most production per unit of effort" trio

If picking a starting batch, do these three:

1. **Real TLS certs** (Tier 1.1) — **DONE (2026-06-30).**
2. **Auth rate-limit + lockout + audit log** (Tier 1.2) — **DONE (2026-06-30).**
3. **A 48–72 h soak to shake out leaks/drift** (Tier 2.4) — next.

That trio takes it from "daily-driver I babysit" to "I can deploy this and walk away on a
network I control." With 1.1 + 1.2 landed, the remaining high-value item is the soak (2.4);
everything else is incremental.
