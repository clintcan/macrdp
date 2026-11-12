# macrdp vs. other open-source RDP servers

Two things live here:

- **[Part 1 — Verified firsts](#part-1--verified-firsts)** — the evidence behind every "first"
  claim made elsewhere in the docs, so they are **citable, falsifiable, and re-verifiable**
  rather than folklore, and so we don't overclaim.
- **[Part 2 — Project comparisons](#part-2--project-comparisons)** — how macrdp actually stacks
  up against the other native macOS RDP servers, written adversarially (steelmanning theirs).

**They rot at different rates.** Part 1 is dated evidence about upstream *absences* and changes
slowly. Part 2 changes whenever either project ships something — re-read it with more suspicion.

**Verified 2026-07-20** by adversarial web research (106 agents; each candidate claim put
to a 3-vote refutation panel — 25 claims → 16 confirmed, 9 refuted) plus a direct read of
FreeRDP's source. The brief was explicitly to *disprove* the claims, not confirm them.

**Re-verified 2026-09-30** by direct source checks: FreeRDP `master` (as of 2026-09-29),
**upstream IronRDP `master`** — which since July has grown UDP, USB and camera crates of its
own, so it now gets a dated section, [§7](#7-upstream-ironrdp--a-dated-timeline) —
lamco-rdp-server, x6nux/macrdp and CGKPK/RDPonMAC. The USB and UDP firsts still stand; for UDP,
upstream IronRDP's server has since caught up (#1954, merged 2026-09-30 — see §2 and §7), so
it's "first known", never "only". **The camera
claim did NOT survive**: gnome-remote-desktop shipped end-to-end camera redirection in GNOME 50,
four months before macrdp (see §3). A later pass the same day also settled smart-card redirection
(not a first) and surveyed the IronRDP-based servers.

> **House rule: always write "as far as is known" / "first known" — never a bare "first"
> or "only".** These are negative-existence claims over a field that was not exhaustively
> enumerated (see [Limits](#limits-of-this-survey)). One claim we might have made was
> already false; assume the others could become false as upstreams move.

---

# Part 1 — Verified firsts

## Verdicts at a glance

| Capability (server direction) | Verdict | Confidence |
|---|---|---|
| **USB redirection** — present a client's USB device as a real local device (MS-RDPEUSB/URBDRC) | **First known** (2026-07-06) — **no longer the only one**: qemu-display's `qemu-rdp` followed on 2026-08-24, presenting to a QEMU guest (see §1) | High |
| **UDP multitransport** — actually carry channel data over UDP (MS-RDPEMT/RDPEUDP) | **First known — no longer the only one:** upstream IronRDP's server carries EGFX over reliable UDP since #1954 (merged 2026-09-30), three months after macrdp (see §2, §7) | High |
| **Camera redirection** — client webcam → **a real OS camera device**, end to end (MS-RDPECAM) | **❌ REFUTED 2026-09-30 — do not claim** (gnome-remote-desktop 50, 2026-02/03; see §3). Only a hedged *macOS-specific* framing survives | High after source read |
| "First/only **native macOS** RDP server" | **❌ REFUTED — do not claim** | High |
| **Microphone redirection** — present a client mic as a real OS *input* device (MS-RDPEAI) | **❌ Not a first — do not claim** (xrdp, gnome-remote-desktop and kmsrdp do it) | High after source read |
| **H.264/EGFX** server-side encoding | **Not a first** — don't claim | High |
| **Smart-card** (MS-RDPESC) server direction | **❌ Not a first — do not claim** (xrdp since 2013; gnome-remote-desktop 51) | High after source read |
| Drive redirection presented as a real mount | **Unadjudicated** — assert nothing | — |

---

## 1. USB redirection, server direction — first known

**Claim:** macrdp presents a *client-redirected physical USB device* as a real local device
on the server host (a flash drive mounts in Finder; a gamepad works).

**Evidence — structural, not just an open issue.** FreeRDP master's `channels/urbdrc/`
contains only `client/`, `common/`, `CMakeLists.txt`, `ChannelOptions.cmake` — **no
`server/` subdirectory**. Its `CMakeLists.txt` has `add_subdirectory(common)` and a
client block gated on `WITH_CLIENT_CHANNELS`, but **no `add_subdirectory(server)`** — so
server code doesn't live elsewhere either. Crucially, FreeRDP *does* ship
`channels/rdpecam/server/`, which proves this absence is meaningful rather than a
repo-layout artifact.

Corroborating: upstream issue [#7558](https://github.com/FreeRDP/FreeRDP/issues/7558)
("server side channel not implemented", opened 2022-01-15) is still **Open**; every 2026
URBDRC CVE is phrased strictly client-direction; and xrdp has explicitly **declined** to
implement it ([discussion #2673](https://github.com/neutrinolabs/xrdp/discussions/2673):
"unlikely to be something the project would want to take on the maintenance for").

**How macrdp's was built: independently, from real traffic.** macrdp's server-side URBDRC
(vendored `ironrdp-server` divergence 16) and its macOS presentation were built by
reverse-engineering packet captures of a real **mstsc ↔ Windows terminal server** session,
alongside the MS-RDPEUSB spec — not on top of anyone else's server code. The collaboration
with upstream below came **after**, to keep macrdp's divergence from IronRDP as small as
possible.

**Upstream IronRDP now has the server side of the protocol — built independently by uchouT;
macrdp coordinated with him afterwards.** IronRDP merged server-direction MS-RDPEUSB *protocol processors*
on **2026-07-01** ([#1394](https://github.com/Devolutions/IronRDP/pull/1394)) and wired them
into `ironrdp-server` on **2026-08-24**
([#1417](https://github.com/Devolutions/IronRDP/pull/1417)), both by uchouT. Once both
implementations existed, macrdp worked with him to converge on upstream: its live bring-up
against real clients produced the `ironrdp-rdpeusb` interop fixes that code sits on
([#1418](https://github.com/Devolutions/IronRDP/pull/1418) lenient USB-3 capability values,
[#1420](https://github.com/Devolutions/IronRDP/pull/1420) full configuration descriptor,
[#1513](https://github.com/Devolutions/IronRDP/pull/1513) `UsbDevice == 0`) plus a decoder fuzz
target ([#1690](https://github.com/Devolutions/IronRDP/pull/1690)); it flagged that #1417 as
merged lacked the per-device capability exchange mstsc requires; and it validated uchouT's fix
([#1711](https://github.com/Devolutions/IronRDP/pull/1711)) on **real mstsc with a redirected
Xbox controller** before it merged on 2026-08-31. The two efforts are complementary, not
rivals.

What upstream provides is a library seam — the application implements `DeviceFactory` /
`UsbRedirDevice`, and nothing in IronRDP creates an OS-level device (no virtual host
controller, usbip, vhci or gadget code anywhere in the tree). So the claim stands as worded —
*presenting* the device — but note the dates: uchouT's protocol processors landed **five days
before** macrdp's redirected drive first mounted (2026-07-06). **Don't claim "first
server-side URBDRC implementation"**; claim the end-to-end presentation.

**A second implementation now exists — say "first known", never "only" (found
2026-09-30).** [qemu-display](https://gitlab.com/marcandre.lureau/qemu-display)'s RDP
server, `qemu-rdp`, merged RDP USB redirection on **2026-08-24** (three commits by uchouT —
the same collaborator as above, and the natural consumer of his own server-side USB code): it
implements IronRDP's `UsbRedirDevice` and
bridges the client's device into QEMU over `usbredir`, so it appears as a real USB device
**inside the QEMU guest**. That is seven weeks after macrdp's first mount (2026-07-06), and it
presents to a virtual machine rather than to the server's own OS — but it is a genuine
server-direction presentation, so "the only working one" (the wording below) is retired.
**Not tested by us** — record it as a peer, not as verified-working. Found by a GitHub code
search for implementations of `UsbRedirDevice`; the other hits were vendored copies of
`ironrdp-server` (AKolenda/ironrdp-winrdp, racko-remote, nyaterm) and an unrelated 2016 usbredir
parser.

**Caveat that softened "only" → "the only working one"** (July, now moot — see above):
[`zoa-kas/xrdp-usb-redirector`](https://github.com/zoa-kas/xrdp-usb-redirector) is a
vendored xrdp 0.9.21.1 fork with a single 2024-03-27 commit *"Add functionality for token
passing and USB device passthrough as RAW"* — 5 commits total, 0 stars, unmodified stock
xrdp README, dormant since 2024-11-20. **Its diff was never read**, and "as RAW" plus its
smart-card/token focus makes it unlikely to be MS-RDPEUSB per spec. It doesn't refute the
claim, but it's enough that "only" was too strong.

**Don't cite as counter-evidence:** the `CHANNEL_URBDRC_SERVER=ON` CMake option name or
the vestigial server `urbdrc.h` header on pub.freerdp.com — both exist without a server
implementation. Cite the source tree and build file.

## 2. UDP multitransport data path — first known

**Claim:** macrdp actually carries channel data (EGFX video; AAC audio on a lossy flow)
over an MS-RDPEMT tunnel on MS-RDPEUDP — not a TCP-side bootstrap stub.

**How it was built:** independently, by reverse-engineering packet captures of a real
**mstsc ↔ Windows terminal server** session alongside the MS-RDPEUDP/RDPEMT specs, in
macrdp's own sans-I/O `vendor/macrdp-rdpeudp` crate plus the vendored server. No other
open-source server-side data path existed to build on (see the evidence below).

**Evidence.** FreeRDP's client **hard-rejects** multitransport via a dedicated
`multitransport_no_udp` stub that unconditionally answers `E_ABORT`; core
`libfreerdp/core/multitransport.c` contains no RDPEUDP implementation (still true
2026-09-30). The RDPEUDP / RDPEUDP2 work (David Fort) stayed **out of tree**.

**Upstream IronRDP is catching up, and is the one to watch.** It gained an RDP-UDP
transport for its **client** in August–September 2026 (see §7), and server-side
*bootstrapping* on **2026-09-28** — but the piece that actually moves channel data (EGFX)
onto the server's UDP tunnel is
[**#1954**](https://github.com/Devolutions/IronRDP/pull/1954) (glamberson), opened
2026-09-10 and **merged 2026-09-30**: an opt-in `RdpServerBuilder::with_udp_transport` that
Soft-Sync-migrates EGFX onto a **reliable** UDP tunnel, tested against mstsc. macrdp's server
carried EGFX over UDP on real mstsc on **2026-06-26** and shipped it in v0.8.15 on
**2026-06-28** — about three months earlier. So the claim is **"first known", never "only"**,
and IronRDP's server should be named alongside it. Two differences, as of #1954 (don't overstate
them — upstream may add either): IronRDP's path is reliable-UDP EGFX only, where macrdp also
carries AAC audio on a **lossy** flow (`--enable-lossy-audio`); and in IronRDP a tunnel that
closes after EGFX has moved ends the connection, where macrdp's watchdog de-migrates EGFX back
to TCP.

Sources: [`multitransport.c`](https://github.com/FreeRDP/FreeRDP/blob/master/libfreerdp/core/multitransport.c),
[issue #10669](https://github.com/FreeRDP/FreeRDP/issues/10669),
[hardening-consulting UDP write-up](https://www.hardening-consulting.com/en/posts/20230109-udp-support-2.html).

## 3. Camera redirection — ❌ NOT a first (refuted 2026-09-30)

**Do not claim macrdp is the first OSS RDP server to present a client webcam as an OS camera.**
**gnome-remote-desktop did it first** — its NEWS for **50.beta (tagged 2026-02-04)** reads "Add
camera redirection support [Pascal; !360]", and it reached stable in **50.0 (2026-03-14)**, four
months before macrdp's 2026-07-20. (Commit `30ddcb4f` "rdp: Add classes for camera redirection",
authored 2022-06-15, merged 2026-01-31.) Verified by source read: `src/grd-rdp-camera-stream.c`
**decodes** the client's H.264 (`grd-decode-session-sw-avc`, plus a hardware path) and publishes a
**PipeWire node with `media.class = Video/Source`, `media.role = Camera`** — exactly the
"decode + register a real OS camera" bar this section used to claim. This was missed in July
because the survey never cleared gnome-remote-desktop (see [Limits](#limits-of-this-survey)).

**How macrdp's was built:** independently, by reverse-engineering packet captures of a real
**mstsc ↔ Windows terminal server** session alongside the MS-RDPECAM spec (vendored
`ironrdp-server` divergence 19), not derived from gnome-remote-desktop's or anyone else's code.
Not being first doesn't change that it's an independent implementation.

**What survives, hedged:** "as far as is known, the first to present a client-redirected webcam as
a native **macOS** camera" — the other native macOS RDP servers (§4) have no camera code
(re-checked 2026-09-30). Pair it with mstsc: MS-RDPECAM is the path mstsc actually uses for
webcams. Never a bare "first OSS RDP server" camera claim.

The July analysis below is kept because its FreeRDP finding still stands.

**⚠️ The protocol-level version of the claim was always false.** FreeRDP **does** ship server-direction
MS-RDPECAM code — `channels/rdpecam/server/` contains `camera_device_main.c` (~29.7 KB)
and `camera_device_enumerator_main.c` (~16.6 KB). So macrdp is **NOT** "the first OSS RDP
server to implement MS-RDPECAM server-side," and saying so invites an easy correction from
anyone who knows the tree.

**What that code actually does (read directly, 2026-07-20).** It is a *channel endpoint*,
not a pipeline. In `device_server_recv_sample_response()`:

```c
pdu.SampleSize = Stream_GetRemainingLength(s);
pdu.Sample     = Stream_Pointer(s);
IFCALLRET(context->SampleResponse, error, context, &pdu);
```

The payload is never processed — only its size and a pointer are extracted and handed to an
application-supplied callback. There is **no video decoding anywhere** (no ffmpeg/avcodec/
openh264) and **no OS device registration** (no V4L2 loopback or equivalent). The caller must
implement decode, presentation, and device exposure.

**So the July claim rested on the end-to-end path** — decode the samples and register a real
camera device with the host OS. FreeRDP doesn't do that; gnome-remote-desktop (above) does, and
did first.

**Upstream IronRDP (checked 2026-09-30):** `ironrdp-rdpecam` landed on **2026-09-02**
([#1870](https://github.com/Devolutions/IronRDP/pull/1870)) with codecs and a `client.rs`
only — the client-direction side, not wired into `ironrdp-client` yet, and no server half.
Not a counterexample.

**Also not a counterexample:** Apache Guacamole's RDPECAM work
([GUACAMOLE-1415](https://issues.apache.org/jira/browse/GUACAMOLE-1415)) is
client-direction — browser → guacd → Windows host. Despite the name, `guacd` acts
architecturally as an RDP *client*, so as a gateway it structurally cannot present a
redirected camera as a local OS camera.

## 4. ❌ "First native macOS RDP server" — REFUTED

**Do not make this claim.** Two independent projects predate this one:

| Project | Created | Stack |
|---|---|---|
| [x6nux/macrdp](https://github.com/x6nux/macrdp) | **2026-03-24** | GPL-3.0; a vendored/patched `ironrdp-server` — **the same lineage as this project**; H.264 + AVC444 via VideoToolbox, HiDPI, NLA |
| [CGKPK/RDPonMAC](https://github.com/CGKPK/RDPonMAC) | **2026-04-26** | Apache-2.0; libxrdp + ScreenCaptureKit; CGEvent/IOKit input; serves mstsc and sdl-freerdp |
| clintcan/macrdp (this project) | 2026-05-13 | Rust on IronRDP |

Both are genuine RDP servers (they terminate the protocol themselves — not VNC bridges or
proxies), and both are **earlier**. macrdp's docs never actually made this claim, so nothing
required retraction — it's recorded here so it's never made by accident.

**They do not threaten claims 1–2 (or the macOS-specific camera framing in §3):** neither implements USB, camera, UDP-multitransport,
drive, or smart-card redirection (source trees re-checked 2026-09-30; neither has pushed
code since). x6nux does have audio and clipboard; RDPonMAC has only stubs for both.

→ Both projects are compared properly in **[Part 2](#part-2--project-comparisons)**.

## 5. Microphone / audio-input redirection (MS-RDPEAI) — NOT a first

**Do not claim macrdp is the first OSS RDP server to redirect the client's microphone — xrdp,
gnome-remote-desktop and kmsrdp already do it, end to end.** Added + verified by direct source read **2026-09-01** (prompted
by macrdp's own MS-RDPEAI feature landing; the mistake would have been easy to make since USB /
UDP above *are* firsts).

**Evidence.** xrdp implements server-direction MS-RDPEAI and presents the client's mic as a
**real recordable OS input device** — a PulseAudio **source** (which is exactly the meaningful
bar, the same one macrdp meets on macOS with a Core Audio input device):

- `neutrinolabs/pulseaudio-module-xrdp` builds **`module-xrdp-source.so`** (audio input),
  alongside `module-xrdp-sink.so` (output). A PulseAudio *source* is a recordable input device,
  so ordinary server-side apps record the client's mic through it.
- `xrdp/sesman/chansrv/sound.c` is a real (non-stub) implementation: `sound_start_source_listener()`
  opens a Unix-domain socket for mic data; `sound_sndsrvr_source_data_in()` handles
  `PA_CMD_START_REC` / `PA_CMD_SEND_DATA` / `PA_CMD_STOP_REC`; it `#include "audin.h"` and calls
  `audin_start()` / `audin_stop()` (the MS-RDPEAI channel), FIFO-buffered.
- Flow: client mic → chansrv (audin / MS-RDPEAI) → `module-xrdp-source.so` → recordable by
  Linux apps on the server. The xrdp wiki states the client→server path is "implemented as per
  [MS-RDPEAI] … interoperable with any RDP client."

**gnome-remote-desktop does it too, since GNOME 46** (commit `28d772a2` "rdp: Add class for
audio input redirection" — authored 2022-07-09, committed 2023-12-16, first in the 46.alpha tag;
not in 45.0). Verified by source read 2026-09-30:
`src/grd-rdp-dvc-audio-input.c` negotiates MS-RDPEAI (A-law preferred, else 16-bit PCM; both
fixed at 44.1 kHz stereo) and publishes a **PipeWire node with `media.class = Audio/Source`**
named "GNOME Remote Desktop Audio Input" — a real recordable input device, fed from an in-process
queue that drops frames older than 200 ms.

**FreeRDP's shadow server negotiates MS-RDPEAI but drops the audio.** `server/shadow/shadow_audin.c`
hands each sample batch to a platform callback (`AudinServerReceiveSamples`), and none of the
X11 / Mac / Win shadow subsystems implements it (checked 2026-09-30) — protocol endpoint only.

**How macrdp's was built:** independently, by reverse-engineering packet captures of a real
**mstsc ↔ Windows terminal server** session alongside the MS-RDPEAI spec (vendored
`ironrdp-server` divergence 25, first live-verified 2026-09-01) — not derived from xrdp,
gnome-remote-desktop, kmsrdp or upstream IronRDP. As with USB, adopting upstream's crate at the
next pin bump is about minimising divergence, not about where the design came from.

**Upstream IronRDP has it too, server side, since September 2026.** The `ironrdp-rdpeai`
protocol crate landed on 2026-08-12 ([#1645](https://github.com/Devolutions/IronRDP/pull/1645))
and its `ironrdp-server` integration on 2026-09-22
([#1946](https://github.com/Devolutions/IronRDP/pull/1946)) — a protocol endpoint (the
application supplies the audio sink), not a device. macrdp plans to adopt it in place of its own
vendored copy at the next IronRDP version bump.

**kmsrdp does it too (Linux, own RDP stack, since 2026-07-17)** — `yamamo-to/kmsrdp`, "Initial
release" 2026-07-17, with its own `rdpcore-rdpeai` crate (the README states "no `ironrdp`
dependency"). It plays the client's mic into a PulseAudio `module-null-sink` named `kmsrdp_mic`,
and apps select **`kmsrdp_mic.monitor`** as their input (`kmsrdp/src/pulse_util.rs`) — a
monitor-of-a-null-sink workaround rather than a first-class source, but a recordable input all the
same. Verified by source read 2026-09-30.

**Among IronRDP-based servers, macrdp is the only one found that presents the mic as a device**
(survey 2026-09-30: GitHub code search for repos implementing an IronRDP server, then a shallow
clone + grep of each for `rdpeai` / `audin` / `AUDIO_INPUT` / `microphone`). Upstream IronRDP
ships only the protocol side — the `RdpeaiServerFactory` / `RdpeaiServerBackend` hook (#1946) —
and no example server uses it. Of the downstream servers checked: **lamco-rdp-server**
(`wayland-rdp` roadmap: "Microphone input — ❌ Not started, P3"), **x6nux/macrdp** (design doc:
"AUDIN … Deferred to a future phase"), and **hypr-rdp, qemu-display (qemu-rdp), ARISU, otto,
wrdp, mrdpd, mRDP, tddy-coder, MobaRust, crosvm, taomni** have no server-side mic code (taomni's
matches are local push-to-talk capture). `racko-remote` is a fork of the IronRDP repo carrying
upstream's `rdpeai.rs` unchanged, not a server that uses it. Clients (`ironrdp-client`, oxideterm,
tabby-rdp) implement the *client* side, which is irrelevant here. Caveat: only repos GitHub's code
index surfaced were checked; a private or unindexed IronRDP server could exist.

**FreeRDP is not the relevant precedent either way:** its `audin` is *client-side* capture (the
`/microphone` flag), and FreeRDP has no device-presenting server, so it neither refutes nor
supports a first claim. xrdp, gnome-remote-desktop and kmsrdp settle it.

**The only defensible framing is platform-specific, with the house-rule hedge:** "as far as is
known, the first to present a client-redirected mic as a native **macOS Core Audio** input
device" (via a from-scratch `AudioServerPlugIn`), pairing with camera redirection for a full
remote webcam **+ mic**. Even that is **unverified** — prior native macOS OSS RDP servers exist
(x6nux, CGKPK; see §4) — x6nux's design doc defers mic input to a future phase (checked
2026-09-30) and CGKPK is display+input only —
so state the macOS angle only with "as far as is known", and **never** a bare "first OSS RDP
server to do mic redirection".

Sources: [kmsrdp `pulse_util.rs`](https://github.com/yamamo-to/kmsrdp/blob/main/kmsrdp/src/pulse_util.rs),
[`grd-rdp-dvc-audio-input.c`](https://gitlab.gnome.org/GNOME/gnome-remote-desktop/-/blob/main/src/grd-rdp-dvc-audio-input.c),
[`shadow_audin.c`](https://github.com/FreeRDP/FreeRDP/blob/master/server/shadow/shadow_audin.c),
[`pulseaudio-module-xrdp`](https://github.com/neutrinolabs/pulseaudio-module-xrdp),
[`xrdp/sesman/chansrv/sound.c`](https://github.com/neutrinolabs/xrdp/blob/devel/sesman/chansrv/sound.c),
[xrdp audio wiki](https://github.com/neutrinolabs/xrdp/wiki/Audio-Output-Virtual-Channel-support-in-xrdp).

## 6. What macrdp should NOT claim

- **H.264/EGFX server-side encoding is not a first** — xrdp and gnome-remote-desktop both do
  server-side H.264.
- **Camera redirection is not a first** — gnome-remote-desktop 50 (§3).
- **Smart-card (MS-RDPESC) server direction is not a first** — xrdp has had it since 2013
  (`sesman/chansrv/smartcard_pcsc.c`, a 66 KB PC/SC daemon stand-in; checked 2026-09-30), and
  gnome-remote-desktop added it in **51.beta (2026-08-19)**, "Add support for smartcard
  redirection [Joan; !406]". macrdp's landed 2026-06-18.
- **Drive redirection presented as a real filesystem mount** was **not adjudicated**. It may well
  be unusual, but assert nothing either way without a source-tree audit.
- [Lamco's comparison page](https://lamco.ai/comparison/) is marketing-quality; the
  verification panel rejected claims resting on it. Don't cite it in either direction.

## 7. Upstream IronRDP — a dated timeline

Upstream IronRDP is the project most likely to overtake these claims: macrdp is built on
it, and since July it has grown UDP, USB and camera crates of its own. Dates are merge dates
on Devolutions/IronRDP `master`; macrdp's are the first live verification and the first
release containing it. Checked 2026-09-30.

| Capability (server direction) | macrdp | Upstream IronRDP | FreeRDP |
|---|---|---|---|
| **UDP — channel data over the tunnel** | EGFX over reliable RDPEUDP, **verified on real mstsc 2026-06-26**; shipped v0.8.15 (**2026-06-28**). Lossy-UDP AAC audio soak-verified 2026-06-29. | **Client** transport: `ironrdp-rdpeudp` + `ironrdp-rdpemt` 2026-08-15/16 ([#1627](https://github.com/Devolutions/IronRDP/pull/1627), [#1626](https://github.com/Devolutions/IronRDP/pull/1626)), async driver 2026-08-18 ([#1687](https://github.com/Devolutions/IronRDP/pull/1687)), RDPEUDP v1/v2 data transfer 2026-09-10 ([#1919](https://github.com/Devolutions/IronRDP/pull/1919)). **Server** bootstrapping 2026-09-28 ([#1951](https://github.com/Devolutions/IronRDP/pull/1951), [#1953](https://github.com/Devolutions/IronRDP/pull/1953), [#1964](https://github.com/Devolutions/IronRDP/pull/1964), [#1965](https://github.com/Devolutions/IronRDP/pull/1965)). Server **EGFX migration onto UDP: [#1954](https://github.com/Devolutions/IronRDP/pull/1954), merged 2026-09-30** (reliable flow only). | None, either side |
| **USB — present the client's device** | Redirected flash drive **mounts on the Mac, 2026-07-06** (real Linux FreeRDP client); shipped v0.8.27 (**2026-07-07**). | Server-direction MS-RDPEUSB **protocol processors 2026-07-01** ([#1394](https://github.com/Devolutions/IronRDP/pull/1394)); `ironrdp-server` integration 2026-08-24 ([#1417](https://github.com/Devolutions/IronRDP/pull/1417)). A library seam (`DeviceFactory` / `UsbRedirDevice`) — **no OS device presentation**. First downstream presenter: qemu-display's `qemu-rdp`, 2026-08-24 (to a QEMU guest; see §1). | None (no `channels/urbdrc/server/`, [#7558](https://github.com/FreeRDP/FreeRDP/issues/7558) open) |
| **Camera — client webcam as a real OS camera** | **Live on real mstsc 2026-07-20** at 1080p/~30 fps; shipped v0.9.0 (**2026-07-20**). | `ironrdp-rdpecam` 2026-09-02 ([#1870](https://github.com/Devolutions/IronRDP/pull/1870)): codecs + **client**-direction state machine only, not yet wired into `ironrdp-client`; no server half. | Server **endpoint** only — hands raw samples to a callback, no decode, no device (§3) |

How to read it:

- **UDP** — upstream built its transport client-first, then wired the server; with #1954 merged
  (2026-09-30) IronRDP's server carries EGFX over UDP too. macrdp's lead is dated (~3 months),
  so say "first known", never "only", and name IronRDP.
- **USB** — upstream's *protocol* processors predate macrdp's first mount by five days, so the
  claim is strictly the **end-to-end presentation**, which upstream deliberately leaves to the
  application. macrdp's own USB was built independently (from mstsc ↔ Windows terminal server
  captures); the collaboration came afterwards, to minimise divergence — see §1.
- **Camera** — upstream is client-direction only. No change — but the claim itself fell to
  gnome-remote-desktop, not to IronRDP (§3).
- **Who built what (checked 2026-09-30).** All of macrdp's redirection implementations — USB,
  UDP, camera and microphone — were built independently by reverse-engineering real mstsc ↔
  Windows terminal server traffic. USB is the one column where macrdp then collaborated with upstream, after the fact,
  to minimise divergence (§1).
  **UDP is independent work** by glamberson (Lamco) and AKolenda: macrdp has no comments or
  reviews on that series or on the RDP-UDP tracking issue
  [#140](https://github.com/Devolutions/IronRDP/issues/140), and never proposed its own
  `ironrdp-rdpeudp` upstream. Its only UDP-adjacent upstream change is
  [#1453](https://github.com/Devolutions/IronRDP/pull/1453) (merged 2026-07-30), which exposes the
  client's multitransport flags on `AcceptorResult`; #1951's server offer does not build on it
  (it captures the same GCC field itself), so the two are complementary halves written
  separately. **Camera is independent work** by mamoreau (#1870). What stays local to macrdp:
  the UDP transport, the USB *presentation* (the macOS virtual host controller) and the camera
  pipeline.

## Limits of this survey

The honest boundary on the surviving claims (1–2): the survey did **not** affirmatively clear
**ogon**, the **Weston/wlroots** RDP backends, **NeutrinoRDP**, or cosmic-ext for server-direction
USB or UDP. The claims rest on FreeRDP + xrdp + gnome-remote-desktop absence-of-evidence plus the
IronRDP timeline in §7 — strong for those projects, but not an exhaustive field survey. Hence "as
far as is known". **The July version of this list included gnome-remote-desktop, and that gap is
exactly what hid the camera refutation (§3)** — an uncleared project is a live risk, not a
formality.

**Cleared or found since July (2026-09-30):**
- **gnome-remote-desktop** — **has camera (50, refutes §3), smart card (51) and mic (46)**; no USB
  and no UDP in its tree or NEWS. Cleared for USB and UDP only.
- **IronRDP-based servers** (found by GitHub code search for IronRDP server implementations, then a
  shallow clone + grep of each): hypr-rdp, ARISU, x6nux/macrdp, kmsrdp (own stack), otto, wrdp,
  mrdpd, mRDP, tddy-coder, MobaRust, crosvm, taomni — **no USB, UDP data path or camera** (their
  only UDP matches are vendored PDU types or `multitransport_flags: None`). Only qemu-display has
  USB (below). cosmic-ext did not surface in the search and remains unchecked.
- **lamco-rdp-server** (IronRDP-based, active) — no UDP, USB or camera in its source; its own
  `Cargo.toml` says server-side drive redirection is "NOT YET AVAILABLE". Cleared.
- **qemu-display / `qemu-rdp`** — has server-direction USB since 2026-08-24 (§1). Found by
  searching GitHub for implementations of IronRDP's `UsbRedirDevice`; the same search is the
  cheapest way to find the next one.

Also note that one supporting line of evidence was voted down during verification: two
claims asserting FreeRDP's merged MS-RDPECAM PR #10258 is client-only were **refuted**,
which is precisely why claim 3 was re-grounded on a direct source read rather than an
API-doc reading.

## Re-verifying this (it will rot)

These are absence claims about actively developed upstreams. To re-check:

1. **USB** — does `channels/urbdrc/` have a `server/` dir, or `add_subdirectory(server)` in
   its CMakeLists? Is [#7558](https://github.com/FreeRDP/FreeRDP/issues/7558) still open?
2. **UDP** — does `libfreerdp/core/multitransport.c` still answer `E_ABORT` via
   `multitransport_no_udp`? Has any RDPEUDP implementation landed in-tree?
3. **Camera (now a NON-first)** — gnome-remote-desktop's `src/grd-rdp-camera-stream.c` (a
   PipeWire `Video/Source`, since 50) is the refutation; it should stay true. For the hedged
   macOS framing, re-check that x6nux/macrdp and CGKPK/RDPonMAC still have no camera code.
4. **Upstream IronRDP** — [#1954](https://github.com/Devolutions/IronRDP/pull/1954) (server EGFX
   over UDP) merged 2026-09-30; has upstream since added lossy-flow audio or TCP de-migration
   (the two differences §2 records)? Does anything in the tree present a USB device to the OS, or
   has `ironrdp-rdpecam` grown a server half? Check who depends on the crates:
   `git grep -l 'ironrdp-rdpeudp\|ironrdp-rdpeusb\|ironrdp-rdpecam' -- 'crates/*/Cargo.toml'`.
5. **Field** — have ogon / gnome-remote-desktop / the IronRDP downstreams grown any
   server-direction redirection channel? Search GitHub code for implementations of
   `UsbRedirDevice` and callers of `accept_finalize_with_multitransport` (IronRDP's server-side
   extension points for USB and UDP).
6. **Microphone (a NON-first)** — does `neutrinolabs/pulseaudio-module-xrdp` still build
   `module-xrdp-source.so`, and does `xrdp/sesman/chansrv/sound.c` still implement
   `sound_start_source_listener()` + `audin_start()`? (This is the evidence that mic
   redirection is *not* a macrdp first — it should stay true; gnome-remote-desktop's
   `grd-rdp-dvc-audio-input.c` and kmsrdp's `pulse_util.rs` are the other two.)

**Part 2 rots faster than Part 1** and on a different trigger: Part 1 tracks *absences* in
upstreams that change slowly, while Part 2 tracks two actively-developed projects. Re-check
their **source trees** (not their READMEs — x6nux's advertises 8 features and omits audio and clipboard, both of which they implement; a README-based comparison of this project was wrong twice) and commit activity before repeating anything from it — particularly the
"where they're ahead" table, which is the part most likely to be out of date (and the part
we'd look worst getting wrong).

Related: [features.md](features.md) (the capability list),
[usb-redirection-feasibility.md](usb-redirection-feasibility.md),
[rdp-udp-multitransport-feasibility.md](rdp-udp-multitransport-feasibility.md),
[camera-extension-setup.md](camera-extension-setup.md).

---

# Part 2 — Project comparisons

## x6nux/macrdp — the closest peer

[`x6nux/macrdp`](https://github.com/x6nux/macrdp) deserves a real comparison rather than a
footnote: it is the **same architectural lineage** (a vendored, patched `ironrdp-server` +
`ironrdp-acceptor`, Rust, native macOS, VideoToolbox H.264) and it **predates this project
by ~7 weeks**. Confusingly, it has the same name. This section is written adversarially —
steelmanning theirs — because "we're better" is not a useful claim, and in several places
it isn't true.

**Facts (re-checked 2026-09-30):** created 2026-03-24, GPL-3.0, 52★/17 forks, last *code*
push still **2026-05-18** — no commits since the July comparison. Ours: created 2026-05-13,
MIT OR Apache-2.0, 41★/9 forks, pushed 2026-09-30.

> **Verified against their SOURCE TREE, not their README (2026-07-20).** This matters: their
> README advertises 8 features and **omits audio and clipboard entirely**, both of which they
> in fact implement. An earlier version of this comparison claimed they had neither — it was
> wrong, because it trusted the README. Read the tree.

### Where x6nux/macrdp is ahead of us

| Their advantage | Our status |
|---|---|
| **AVC444 shipped** ("pixel-perfect color", RDP 10) — `yuv444_split.rs` (19 KB) | **Not wired here — parked deliberately, not unfinished.** `src/avc444.rs` has spec-compliant split/combine + roundtrip tests; upstream `ironrdp-egfx` already exposes `send_avc444_frame`. Parked on a *measurement*: VideoToolbox shares one hardware encoder block (two sessions ≈ **1.02× throughput**, effectively serial), so AVC444 costs ~2× encode wall-clock — fine at 1080p/60 (~10 ms/frame), **doesn't fit 4K/60** (~39 ms vs 16.6 ms). Plan is opt-in `--avc444` with that caveat. **They run on the same Apple Silicon and inherit the identical constraint** — they shipped it anyway. |
| **openh264 software encoder** (13.8 KB) — a VideoToolbox-independent H.264 path | **We have none — H.264 is VideoToolbox-only.** Calibrate this: macrdp is macOS-only and VideoToolbox H.264 encode exists on every supported Mac, so "hardware encode unavailable" is close to a null case, and we still have a full *software* **legacy** path (`rfx.rs`, `nscodec.rs`, `bitmap.rs`) that non-AVC420 clients fall back to automatically. The more interesting angle is **AVC444**: it needs two H.264 streams, and we parked it because VideoToolbox serializes them (1.02×, one shared hardware block) — but a software encoder runs on CPU cores, so VT-for-main + openh264-for-aux could give real parallelism the VT-only budget can't. That may be why they ship both (**unverified — not checked whether they actually pair them**). Worth investigating if AVC444 is ever revisited. |
| **Lock-screen capture** (CoreGraphics fallback) | We have none — but see [known-quirks.md](known-quirks.md): the lock screen renders on the *physical* panel and macOS blocks synthetic input to the login window, so copying this yields a screen you still cannot type into. Lower value than it appears. (Different feature, for completeness: since v0.9.8 macrdp can *lock* a headless Mac when the client leaves and unlock it on return — opt-in, experimental.) |
| **A richer GUI** — a Tauri app with Dashboard, **Statistics charts**, an **in-app log viewer**, Settings, Permissions and a tray popover, plus **connection and daily-traffic history in SQLite** (`database.rs`) | **We have a GUI too** — the menu-bar app opens a full **nine-tab Settings window** (Status · Connection · Video · Audio · Display · Input · Redirection · Advanced · Permissions): live server CPU/RAM/uptime and the connected client, live bitrate/RTT/fps (with the opt-in stats endpoint), one-click installers for the camera extension and mic driver, and Apply/Revert. What theirs has that ours lacks: **charts, persisted history, and a log viewer** (ours opens the log file externally). |
| **Live config changes without a restart** — the GUI pushes frame rate, bitrate, resolution, encoder, chroma mode, log level and credentials to the running server (`ConfigUpdate`; settings stored as TOML) | Ours applies a batch of changes with **one server restart** (Apply), which drops a connected session. |
| Earlier (2026-03-24 vs 2026-05-13), more stars | — |

### Where we're comparable — both implement it

Corrections to an earlier, README-based version of this table, which wrongly claimed these were
missing on their side:

- **Audio output** — both have it (`macrdp-audio`, ~11 KB; ours `audio.rs` 41 KB + `aac.rs` 16 KB).
  Ours adds opt-in AAC compression and can carry audio on a lossy UDP flow; theirs is PCM-focused.
  (Audio *input* — the client's mic — is ours alone; see below.)
- **Clipboard** — both have it (theirs ~53 KB incl. `transfer.rs`, `pasteboard.rs`, `file.rs`,
  `html.rs`; ours ~110 KB incl. `clipboard_rich.rs`). Format-by-format: **text**
  (CF_UNICODETEXT) ✅ both; **images** ✅ both — ours PNG↔DIB (and DIBV5 accepted from Windows),
  theirs accepting `public.png`/`tiff`/`jpeg`→DIB; **file lists** (FileGroupDescriptorW) ✅ both;
  **HTML** ✅ both — theirs since before July, **ours since v0.9.9 (2026-09-29)**, which closed
  the one clipboard gap the July comparison found. Ours also carries **RTF** (Word's native
  rich format, and the only rich format some Word builds offer), both directions, live-verified
  on Windows 11 mstsc. Their tree has no RTF.
- **Adaptive bitrate**, **hardware H.264 via VideoToolbox**, **HiDPI/Retina capture**, **NLA/CredSSP
  + auto TLS**, **RemoteFX (RFX)** — present on both. **But not NSCodec — see below.**

### Where macrdp is differentiated

Each of the following is **absent from their entire source tree** (whole-tree search for
`rdpdr`, `urbdrc`, `rdpecam`, `smartcard`/`scard`, `rdpeudp`/`multitransport`,
`virtual_display`/`cgvirtual` — the only `usb` hit is a UI status-bar string):

- **Device redirection — the whole category:** **USB** (a redirected drive mounts in Finder;
  gamepads work), **camera** (client webcam → a real macOS camera), **microphone** (client mic →
  a real macOS input device, since v0.9.10 — their design doc defers it: "AUDIN … Deferred to a
  future phase"), **drive** (client drive as a real read-write NFS volume), **smart card**
  (client's card usable by macOS PC/SC apps).
- **Headless operation** — `CGVirtualDisplay` virtual displays plus `--capture-primary` /
  `--detach-primary` / `--shield-primary` blanking, so the Mac serves a desktop with no monitor
  attached (`--shield-primary` keeps it lockable), plus opt-in lock-on-disconnect.
- **UDP multitransport** (MS-RDPEMT/RDPEUDP), including lossy-flow audio.
- **Production hardening** — per-IP rate-limiting + escalating lockout, a structured JSON audit
  stream for SIEM, a health-check watchdog, bounded log rotation, mstsc blank-recovery, and
  RTT-aware rate control for VPN/high-latency links.
- **Input depth** — non-US keyboard layouts auto-detected from the client, optional Ctrl→Cmd
  remapping, symbolic-hotkey workarounds, an app-switcher HUD.
- **NSCodec legacy codec** — `nscodec` is **absent from their entire tree**; their encoder set is
  `bitmap`/`fast_path`/`rfx`. This matters concretely: **Microsoft Remote Desktop / Windows App on
  macOS advertises *only* NSCodec** in its legacy bitmap codec list, so without it that client
  falls back to raw/RLE `BitmapUpdate` — bandwidth-heavy. macrdp serves Apple's own RDP client
  better on the legacy path. (Both have H.264, which those clients do negotiate, so it bites on
  the fallback path.) Note the provenance: macrdp **contributed this upstream** — IronRDP PR
  **#1332, merged 2026-06-01** — where the `nscodec` module existed but was *dead code, never
  wired up*; the contribution was the handler, encoder-codec slot, dispatch variant, selection
  arm and server-side `CodecProperty::NsCodec` match. Their vendored `ironrdp-server-gfx` fork
  predates or omits that merge, so it is available upstream and simply unadopted.
- **Licensing** — MIT OR Apache-2.0 vs their GPL-3.0; materially different for embedding or commercial use.
- **Upstream contribution posture.** Both projects vendor *patched* IronRDP forks — only one feeds
  fixes back. Measured 2026-10-01 via the GitHub API: **macrdp's author has 24 PRs to
  Devolutions/IronRDP, all 24 merged (merge dates 2026-05-21 → 2026-09-30); x6nux has 0.** Merged work
  includes the RDPSND audio keep-newest fix (**#1276**), `SuppressOutput`/`RefreshRectangle`
  handling (**#1319**), the NSCodec encoder + selection (**#1332**), EGFX capability-decode
  tolerance (**#1298**), three CLIPRDR fixes (**#1299/#1300/#1301**), QOI bitmap fixes
  (**#1335/#1341**), RDPSND format negotiation (**#1359**), CLIPRDR request/response
  correlation (**#2053**, 2026-09-30) and its test-helper follow-up (**#2056**), acceptor field surfacing
  (**#1373/#1397/#1404/#1453**), the Server Auto-Reconnect Cookie (**#1405**), USB-PDU
  fixes (**#1418/#1420/#1513**), a pre-TLS denial-of-service fix (**#1556**) and a USB-decoder
  fuzz target (**#1690**) —
  several of which let macrdp *delete* vendored forks entirely. This is a real difference in
  kind, not a scoreboard: fixes landed upstream benefit every IronRDP downstream **including
  x6nux**, and the NSCodec gap above is exactly that — sitting upstream, contributed here,
  simply unadopted there. Re-verify with:
  `gh api 'search/issues?q=repo:Devolutions/IronRDP+is:pr+author:<user>'`.

### Fair summary

Both are genuine, actively-built macOS RDP servers sharing a lineage, and the honest gap is
**narrower than a README comparison suggests** — they have audio output, clipboard, AVC444, a software
encoder, and a richer GUI (charts, history, a log viewer, live config changes) — though both
projects have one. The real distinction is **device redirection, headless operation,
UDP transport, and operational hardening**: macrdp is a remote-desktop *platform*, theirs is a
polished remote *display*. Neither supersedes the other, and for "see and drive my Mac with good
color and a nice UI" theirs is a reasonable — arguably better-presented — choice.

## CGKPK/RDPonMAC — the other native macOS RDP server

[`CGKPK/RDPonMAC`](https://github.com/CGKPK/RDPonMAC) (created 2026-04-26, Apache 2.0 per its README) is a
genuine native macOS RDP server built on a different stack: **libxrdp + ScreenCaptureKit**,
with `CGDisplayCreateImage` login-screen fallback, `CGEvent`/IOKit HID input injection, and
verified service to both mstsc and sdl-freerdp. It terminates RDP itself — not a VNC bridge,
not a proxy.

It is **display + input only**: no audio, clipboard, or any redirection channel. (Its tree
does contain `RDPAudioBridge.c` / `RDPClipboardBridge.c` — both are stubs marked "re-implement
… in Phase 8", and its README lists clipboard and audio as planned.) Its one
notable capability macrdp lacks is the **login-screen capture fallback** — see the
capture-primary lock quirk in [known-quirks.md](known-quirks.md) for why that turns out to
matter less than it sounds (a remotely-visible lock screen still cannot be typed into).

Last pushed **2026-04-27** (re-checked 2026-09-30) — dormant since. Its README states
Apache 2.0, though GitHub detects no license file. Treat the above as a snapshot.

## Scope of Part 2

Deliberately limited to the two projects that were actually examined. **No feature matrix
against xrdp / gnome-remote-desktop / ogon appears here on purpose** — Part 1 checked them only
channel-by-channel for specific claims (and [Limits](#limits-of-this-survey) records what was
never cleared), so a tidy comparison grid would imply verification that does not exist.
