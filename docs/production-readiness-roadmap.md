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
3. **Document the posture honestly — DONE.** Even hardened, internet-facing RDP is a bad
   idea for *any* server — the production answer is "behind a VPN or an RD Gateway." This is
   stated in the README **§Production readiness** (the "Short version" line: don't put it on a
   public IP, and the trusted-LAN-scope limitation: put internet-facing RDP behind a VPN / RD
   Gateway), and reinforced by a **Network exposure** note next to the LAN-bind examples in
   **§Examples**. See also `@docs/macos-gotchas.md` (port 3389 privileged → 3390 default).

## Tier 2 — Reliability / unattended operation

4. **A real multi-day soak.** *(highest confidence per hour.)* The biggest unknown for
   "leave it running" is leaks/drift over time. Known suspects: the *audio long-session
   drift* item, and documented SCStream / NFS-mount leaks on hard kill (`SIGKILL` skips
   `Drop`). Run a 48–72 h soak (idle + active, with reconnect cycles) and fix what it
   surfaces.
   - **Status — foundation core PASSED a 31 h leak/drift soak; full 48–72 h on v0.8.22+ still
     pending.** The soak run (started 2026-07-01 18:39, **pre-v0.8.22 / pre-ARC** build, 31 h /
     1861 one-minute samples; data recovered on a clean re-copy after a first transfer came back
     zero-filled) shows the **foundation core is clean over time, not just alive:**
     - **No memory leak** — RSS bounded 18–88 MB, tracking activity (88 active at start, down to
       18 idle, back to ~60–71 active), ending *lower* than it started. No threads/fds/SCStreams/
       NFS-mounts/log growth either; **single process the whole run** (no crash/restart/hang).
       Corroborated independently by `pmset` (the `caffeinate` assertion is `-w`-tied to macrdp's
       pid and held unbroken 30 h 45 m) + ~36-day machine uptime.
     - **0 panics.** The 55 `Connection error`s are all per-connection (write-all / accept_begin /
       CredSSP) — the normal client-drop / half-open-probe signatures, non-fatal.
     - **v0.8.21 auth-guard fix FIELD-VALIDATED.** The run captured the before/after: the 17
       lockout rejects of a legitimate LAN client (escalating to ~239 s) are all **pre-fix**
       (06-30 + 07-01 02:xx, before the 18:39 build swap) — this *is* the false-lockout that
       surfaced the v0.8.21 fix. The **post-fix soak window had ZERO lockouts** and 14 perfectly
       balanced accept/disconnect pairs. (The overnight escalation cluster is the "took a few
       tries while I was out" incident — pre-fix, now fixed.)
   - **Why still not "DONE":** (a) 31 h is short of the 48–72 h target; (b) the run predates
     v0.8.22, so its **new features were NOT exercised** — the blank-recovery detector (runs
     per-QoE-callback, can drop the connection) and the ARC auto-reconnect cookie. So the
     *foundation core* (capture → encode → ship → audio → input steady state) is
     **production-validated for leak/drift + no-crash longevity over 31 h**; a **full 48–72 h
     re-soak on v0.8.22+**, biased toward reconnect cycles, is still needed to (1) extend the
     duration and (2) validate the v0.8.22 deltas (esp. blank-recovery false-positive resistance
     over hours). Two logging notes for that run: harden the soak logger to `fsync`/`F_FULLFSYNC`
     periodically (or tee key events to the crash-durable macOS unified log) so a transfer/
     interruption can't zero-fill the record; and the `multitransport`/`audio_dvc` "GREEN"
     status lines log at WARN — demote to INFO/DEBUG to cut soak noise.
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
3. **A 48–72 h soak to shake out leaks/drift** (Tier 2.4) — **foundation core PASSED (31 h).**
   A 31 h run confirmed no memory/fd/thread/stream/mount leak, 0 panics, and **field-validated
   the v0.8.21 auth-guard fix** (pre-fix lockouts captured; post-fix window clean); see Tier 2.4
   above. Remaining to fully close it out: a **48–72 h re-soak on v0.8.22+** exercising reconnect
   cycles (ARC cookie + blank-recovery detector).

That trio takes it from "daily-driver I babysit" to "I can deploy this and walk away on a
network I control." With 1.1 + 1.2 landed, the foundation core is soak-validated for leak/drift
+ no-crash longevity (31 h); closing out Tier 2.4 (full 48–72 h on v0.8.22+) is the remaining
high-value item; everything else is incremental.
