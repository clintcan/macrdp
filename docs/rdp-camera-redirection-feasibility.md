# Client Camera (Webcam) Redirection on macrdp — Feasibility Notes

> **STATUS: SHIPPED.** Camera redirection landed in **v0.9.0 (2026-07-20)** — a
> client-redirected webcam now presents as a real macOS camera. This document is kept
> as the original *research/scoping* record (written 2026-07-16, before any code) and
> is **no longer the source of truth for how it works**. For that, see:
> **[camera-extension-setup.md](camera-extension-setup.md)** (setup + the four silent
> CoreMediaIO failure modes — read this before touching the camera code),
> [features.md](features.md) (what it does), [architecture.md](architecture.md)
> (the module map), and the vendored `ironrdp-server` divergence (19) log.

*Original research notes, 2026-07-16 — exploratory, written when nothing was
implemented. Motivated by the ground-truth finding below.*

> **Why this doc exists — the webcam-over-mstsc question is finally settled.**
> The long-running "why doesn't a redirected webcam show up over mstsc?" thread
> is closed by a decrypted real-Windows capture (`~/mstscpcap_camera3.pcapng`,
> mstsc → a Windows 10 Pro RDS host, camera confirmed live on the remote side).
> Decrypting the TCP control channel (own RDP cert + forced RSA-KX; see
> `project_usb_redirection_feasibility` memory for the method) and enumerating
> every dynamic virtual channel proved:
> - **The webcam is NOT redirected over USB (`URBDRC` / MS-RDPEUSB).** There is
>   zero URBDRC in the session, yet the camera streams **50 MB client→server**.
>   This is the proof behind the earlier `--udp-migrate-usb` experiment's
>   negative result: mstsc refuses every URBDRC camera transfer (`0x8007001f`)
>   because the video never rides USB. **No amount of USB-redirection work can
>   ever surface an mstsc webcam** — it is a *missing protocol*, not a USB bug.
> - **The webcam rides a dedicated RDP video-redirection channel** — specifically
>   **MS-RDPECAM** (Video Capture Virtual Channel Extension), whose enumeration
>   channel is `RDCamera_Device_Enumerator`. That is the protocol to implement.
> - **The camera rode the reliable UDP multitransport tunnel** (no DTLS/lossy flow
>   present) — a transport macrdp already implements for EGFX.
>
> **Correction (from MS-* spec research, 2026-07-16):** the decrypted *TCP* control
> census showed `Microsoft::Windows::RDS::Video::Control/Data::v08.01` +
> `::Geometry::v08.01`, and an earlier read wrongly attributed the webcam to
> `Video::Data`. Those are **MS-RDPEVOR** (Video *Optimized Remoting*) + **MS-RDPEGT**
> (Geometry) — and MS-RDPEVOR is defined as **server→client** (it streams the *host's*
> rapidly-changing desktop graphics *down to the client*, dependent on the EGFX/
> Graphics channel). So those channels carried the desktop's optimized video
> downstream, **not** the webcam. The webcam's 50 MB client→server was simply **not
> in the TCP census** because the MS-RDPECAM camera channel was created and carried
> **inside the reliable UDP tunnel** (after the DRDYNVC Soft-Sync migrated onto UDP),
> where the TCP-side decryption can't see it — which is exactly why no
> `RDCamera_Device_Enumerator` appeared on TCP. The tunnel's own TLS ServerHello
> selected `0x009d` (RSA-KX), so `rdpkey.pem` *could* decrypt it, but tshark doesn't
> chain RDPEUDP→TLS, so reading the camera PDUs from the capture needs custom tooling
> (see "Can the capture decode the video?" below). This does not change the plan —
> macrdp is the *server* and should advertise **MS-RDPECAM** regardless.
>
> So client-webcam-into-a-Mac-session is a **reachable feature**, not a dead end
> like the USB path was. This doc scopes it.

## TL;DR

- **macrdp could, in principle, receive a client's redirected webcam and present
  it as a real macOS camera.** Unlike generic USB redirection, this rides a
  **documented Microsoft Open Specification (MS-RDPECAM)** over a plain dynamic
  virtual channel — no reverse-engineering of an encrypted capture needed.
- **macrdp is the *presenting/consuming* side** (the client owns the physical
  camera). Two layers, same shape as every other redirection feature:
  1. **RDP protocol** — MS-RDPECAM server side (open `RDCamera_Device_Enumerator`,
     negotiate a media format, receive the sample stream). Not in IronRDP; we'd
     write it, the same pattern as server-direction RDPDR / RDPESC / RDPEUSB.
  2. **macOS presentation** — a **CoreMediaIO Camera Extension** (`CMIOExtension`,
     a signed System Extension) publishing a virtual camera every Mac app can use.
- **Most of the hard infrastructure already exists in macrdp:** DVC channels (EGFX,
  audio, RDPDR, URBDRC all ride them), the **reliable UDP multitransport tunnel +
  inbound tunnel→DRDYNVC path** the camera uses, and **VideoToolbox** (we already
  H.264-*encode*; the camera needs H.264-*decode*, the same framework in reverse).
- **The genuinely new, hard piece is the macOS virtual camera** (Camera Extension +
  app↔extension frame IPC). Its entitlement
  (`com.apple.developer.system-extension.install`) is **self-serviceable** with a
  Developer ID — no Apple-grant gauntlet like the USB host-controller entitlement.
- **Recommendation:** treat it as a **major, multi-week feature** roughly
  RDPDR/URBDRC-scale plus the camera-extension side — but squarely in macrdp's
  demonstrated wheelhouse, and very plausibly **another "first for an OSS RDP
  server."** Gate a real build on a small Phase-0 spike: advertise
  `RDCamera_Device_Enumerator` from macrdp and confirm a modern mstsc client
  answers with a `DEVICE_ADDED_NOTIFICATION` for its webcam.

## Context — what macrdp does today

macrdp redirects devices by **device class / protocol**, presenting each through a
plug-in point macOS already exposes:

- **Drive redirection** (RDPDR / MS-RDPEFS) → a real NFS mount.
- **Smart-card redirection** (RDPDR / MS-RDPESC) → a user-space PC/SC IFD handler.
- **Generic USB redirection** (MS-RDPEUSB / URBDRC) → a user-space virtual USB host
  controller. Mass storage + HID verified; **but mstsc does not send a webcam over
  this path** (proven above).

Camera redirection is the natural next device class, and — like smart cards and
drives, and unlike generic USB — it rides a **question/answer protocol** into a
macOS plug-in slot (CoreMediaIO), so there is no raw hardware to synthesize at the
USB level. That is exactly the "device-class beats generic USB" pattern macrdp's
redirection strategy is built on.

## The two layers any solution needs

In RDP, the **client** redirects its camera to the **server** — so macrdp is the
**presenting/consuming** side.

1. **RDP protocol layer** — open the camera channel, negotiate a format, receive the
   video sample stream (MS-RDPECAM).
2. **macOS presentation layer** — expose the decoded stream as a real macOS camera
   so Photo Booth / Zoom / QuickTime / any AVFoundation app can select it.

## Layer 1 — the RDP protocol (`MS-RDPECAM`)

MS-RDPECAM — *Remote Desktop Protocol: Video Capture Virtual Channel Extension* — is
the modern, publicly documented camera-redirection protocol (introduced ~Windows 10
1903 / Server 2019). It rides **DRDYNVC** (dynamic virtual channels), so it slots
straight into macrdp's existing DVC server framework (`DrdynvcServer::create_channel`,
the same seam EGFX / audio / URBDRC use).

**Channel topology (two kinds of DVC):**

- **`RDCamera_Device_Enumerator`** — one per session, **server-opened**. Version
  negotiation + device add/remove notifications ride it.
- **Per-camera device channels** — one per redirected camera; the channel name is
  handed to the server in the device-added notification. Format negotiation, stream
  control, and the **actual video samples** ride these.

**Message flow (per MS-RDPECAM §2.2 / §3; semantics — see the spec for exact
opcodes and struct layouts):**

1. **Enumerator handshake.** Server opens `RDCamera_Device_Enumerator` →
   version-select request/response → the client sends a **Device Added Notification**
   for each available camera (device id + the per-device channel name). (Device
   Removed Notification handles unplug.)
2. **Per-device open + capability read.** Server opens the named per-device channel,
   then:
   - **Stream list** request/response — how many media streams the camera exposes.
   - **Media-type list** request/response — for each stream, the supported
     `CAM_MEDIA_TYPE_DESCRIPTION`s: **format** (H.264, MJPG, NV12, I420, YUY2,
     RGB24/32), **resolution**, **frame rate**.
   - Optional **property** list/get/set (brightness, focus, etc.).
3. **Start streaming.** Server sends **Start Streams** naming the chosen media type;
   the client then **pushes Sample PDUs** (one per frame: payload + timestamp) on the
   device channel. Sample-error and stop/pause/resume messages round it out.

**Formats — pick H.264.** Modern mstsc offers **H.264** (the client hardware-encodes
the webcam), which is bandwidth-cheap and exactly what VideoToolbox decodes. MJPG and
the raw formats (NV12/I420/YUY2) are fallbacks. macrdp advertises the formats it can
decode and picks H.264 when present.

**Server-direction MS-RDPECAM is not in IronRDP** — we'd write the PDU layer + a
server processor ourselves, the same pattern as server-direction RDPDR (divergence
11), RDPESC, and RDPEUSB/URBDRC (divergence 16): a small PDU module + an
`RdCameraServer` `DvcProcessor` riding a new `Option<Box<dyn CameraFactory>>` seam on
`RdpServer`, byte-identical when the feature is off. **CORRECTION (2026-07-20, from
a primary-source Phase-1 scoping pass — supersedes an earlier "unprecedented / no OSS
server" claim here): FreeRDP HAS a full working `rdpecam` *server*** —
`channels/rdpecam/server/{camera_device_main.c,camera_device_enumerator_main.c}` +
shared struct encode/decode in `channels/rdpecam/common/` (and the mirror client in
`channels/rdpecam/client/`). So this is **not** unprecedented and **not** a
reverse-engineer: Phase 1 is *mirror FreeRDP's known-good server state machine + wire
format*. (macrdp presenting the camera on macOS via CoreMediaIO is still likely a
first for an OSS RDP server, but the *protocol* half has a reference.)

**Transport is already solved.** The camera channel is a normal DVC, so it works
over **TCP DRDYNVC** out of the box (macrdp receives inbound DVC data on TCP today).
For the real 50 Mbit-class stream, mstsc Soft-Sync-migrates the channel onto the
**reliable UDP multitransport tunnel** — which macrdp **already implements**,
including the **inbound tunnel → DRDYNVC path** (multitransport M5c step 3b: inbound
`RDP_TUNNEL_DATA` → `DrdynvcServer::process`). So a migrated camera channel's samples
would ride macrdp's existing tunnel plumbing. (First cut can keep it on TCP for
simplicity — with the caveat that a big upstream video stream on the shared TCP
socket writer contends with EGFX/audio, the same A/V-contention lesson as RDPDR
copy-to-drive; UDP is the production answer.)

## Layer 2 — presenting a camera on macOS

macOS exposes virtual cameras through **CoreMediaIO**. Two mechanisms, one viable:

| Option | Mechanism | Status / cost |
|---|---|---|
| **CoreMediaIO DAL plug-in** | A `.plugin` in `/Library/CoreMediaIO/Plug-Ins/DAL/` | **Legacy / dying.** Deprecated; **does not load in apps built with the hardened runtime + library validation** (which is most modern apps — Safari, Zoom, FaceTime). Don't target it. |
| **Camera Extension (`CMIOExtension`)** ⭐ | A **System Extension** of type "Camera", embedded in the app, activated via `SystemExtensions.framework` | **The viable route** (macOS 12.3+). Publishes a virtual camera visible to **every** app system-wide. This is how OBS Virtual Camera and friends work post-Catalina. |

### The Camera Extension route (concretely)

- A `CMIOExtensionProvider` exposes a `CMIOExtensionDevice` with a
  `CMIOExtensionStream`; a stream **source** pushes `CMSampleBuffer`s (backed by
  `CVPixelBuffer`s) that AVFoundation delivers to any consuming app.
- Packaged as `Contents/Library/SystemExtensions/…appex` inside the signed app;
  activated with an `OSSystemExtensionRequest`; the user approves it **once** in
  System Settings → Privacy & Security (survives rebuilds if the identity is stable —
  same signing-identity discipline as the LaunchAgent/TCC notes in
  `docs/macos-gotchas.md`).
- **Entitlement:** `com.apple.developer.system-extension.install`. This is
  **self-serviceable** for a Developer ID app (addable in Xcode's capabilities /
  the provisioning profile) — **not** an Apple-grant gauntlet like
  `com.apple.developer.usb.host-controller-interface` was, and nowhere near the
  DriverKit `transport.usb` wall. So the permission is a minor hurdle, not a risk.
- **The new plumbing = app ↔ extension frame IPC.** The extension runs in its **own
  system-managed process**, so macrdp (the RDP server) has to hand it decoded frames
  across a process boundary — a shared `CMSimpleQueue` / IOSurface ring, or an XPC
  channel, from macrdp → the extension's stream source. This is the trickiest genuinely
  new piece, but it fits macrdp's established **"quarantine the messy part in its own
  process behind a narrow boundary"** convention exactly (the `ifd-handler/` cdylib,
  the `macrdphud` Swift helper, the `usb_spike.m` SPI boundary).

### Decode is already in the toolbox

The camera samples arrive H.264 (or MJPG / raw). macrdp already drives **VideoToolbox
for H.264 *encode*** (`src/videotoolbox.rs`); the camera needs a VT **decompression**
session — the same framework in reverse — emitting `CVPixelBuffer`s ready to hand to
the extension. MJPG decodes via VT/vImage; raw NV12/I420/YUY2 wrap directly into a
`CVPixelBuffer`. No new heavy dependency.

## What it would take for macrdp (concretely)

A phased build, each phase separately testable, mirroring how USB redirection was
staged:

1. **[Phase 0 — cheap gate] ✅ BUILT (2026-07-16), pending a live mstsc test.**
   Advertise `RDCamera_Device_Enumerator` from a throwaway server processor and
   confirm a **modern mstsc client** (Win10 1903+ / Win11, "Video capture devices"
   enabled in Local Resources → More) answers with a `DEVICE_ADDED_NOTIFICATION` for
   its webcam. This proves the client speaks MS-RDPECAM to macrdp before any
   decode/presentation work. **No entitled build, no macOS camera code** — pure
   protocol, like the URBDRC Phase-3.0 observe slice.
   *Implemented:* `crates/ironrdp-server/src/rdcamera.rs` (clintcan/IronRDP) (`RdCameraServer`
   `DvcProcessor` — SHARED_MSG_HEADER parse, SelectVersion negotiation, and the
   `DEVICE_ADDED_NOTIFICATION` log = the GREEN signal), the `camera_factory` DVC seam
   (divergence 19: field + `new()` param + `attach_channels` advertise +
   `with_camera_factory`), the cross-platform `src/camera/mod.rs` (`MacCamera`
   factory, no macOS code), and the `--enable-camera-redirection` flag /
   `ENABLE_CAMERA_REDIRECTION` env. Byte-identical when off. **Next: set
   `ENABLE_CAMERA_REDIRECTION=1`, connect a real mstsc with the webcam checked, and
   watch for `MS-RDPECAM DEVICE_ADDED_NOTIFICATION … (Phase-0 GREEN)` in the log.**
2. **[Phase 1]** MS-RDPECAM PDU layer + `RdCameraServer` `DvcProcessor` (vendored
   `ironrdp-server` divergence, riding a new `camera_factory` seam): enumerator
   handshake → per-device channel → stream/media-type enumeration → **Start Streams**
   → log incoming Sample PDUs. Verifies the full negotiation and that frames arrive.
   Runs over **TCP** first (no UDP dependency).
3. **[Phase 2]** Decode the sample stream (VideoToolbox H.264 decompression) →
   `CVPixelBuffer`s; dump to disk / a debug window to confirm a live image.
4. **[Phase 3 — the macOS piece]** Camera Extension (`CMIOExtension`) + the app↔
   extension frame IPC; the redirected webcam appears as a selectable camera in
   Photo Booth / Zoom. This is the bulk of the genuinely new work.
5. **[Phase 4]** UDP: let the channel Soft-Sync-migrate onto the reliable tunnel
   (reuse the existing inbound tunnel path) so the stream doesn't contend with EGFX
   on the TCP writer; lifecycle (device add/remove, stop-on-disconnect, teardown).

## Don't be misled by the `RDS::Video` channels — they aren't the camera

The decrypted capture's TCP control channel showed
`Microsoft::Windows::RDS::Video::Control/Data::v08.01` + `::Geometry::v08.01`, and it
is tempting to treat those as the camera transport. They are **not**. They are
**MS-RDPEVOR** (Video *Optimized Remoting*) + **MS-RDPEGT** (Geometry), and per the
MS-RDPEVOR spec they redirect *"rapidly changing graphics content as a video stream
**from the remote desktop host to the remote desktop client**"* — i.e. **server→client**
desktop-video optimization, dependent on the EGFX/Graphics channel. They carried the
*host's* desktop video downstream to the mstsc client, not the webcam upstream.

The actual camera channel (**MS-RDPECAM**, `RDCamera_Device_Enumerator` + a per-device
channel) never appeared in the TCP census because it was created and carried **inside
the reliable UDP multitransport tunnel**, after the DRDYNVC Soft-Sync migrated the
dynamic-channel layer onto UDP — encrypted where the TCP-side decryption can't see it.

None of this changes the plan: **macrdp is the server**, so *macrdp* chooses what to
advertise. Advertise the **documented MS-RDPECAM** `RDCamera_Device_Enumerator` and
test against a client whose Windows build speaks it (Win10 1903+ / Win11 mstsc, the
common case today). Building against the published spec with a matching live client is
far cheaper than trying to recover the protocol from the encrypted tunnel.

## Can the capture decode the video? (Possible, but not needed)

Yes in principle, no in practice-without-effort:

- **The key would work.** The reliable UDP tunnel is TLS, and its ServerHello selected
  `0x009d` (`TLS_RSA_WITH_AES_256_GCM_SHA384`, RSA key exchange, no forward secrecy) —
  so `rdpkey.pem` can decrypt the tunnel, exactly as it decrypts the TCP side.
- **tshark can't do it.** Wireshark/tshark do not chain the custom RDPEUDP transport
  into TLS, so nothing dissects the tunnel automatically.
- **Full decode = writing a partial RDP client for the recording:** parse RDPEUDP
  headers and reassemble the reliable byte stream (in-order, dedup) → extract + decrypt
  the TLS records → un-wrap MS-RDPEMT tunnel data → DRDYNVC → find the MS-RDPECAM DVC →
  parse the media-type negotiation → concatenate the sample payloads → H.264/MJPEG
  decode. A few days of work, and it yields *one recorded session*, not a capability.
- **It is not a prerequisite.** Both protocols are fully documented (below), so
  implementing against the spec with a live client (Phase 0) is the real path. If you
  ever want a *cheaper* slice than full video decode, stop after the DRDYNVC layer and
  just read the camera **channel name + media-type negotiation** to confirm the codec —
  but a live Win11 test gets the same answer with less effort.

## Distribution / packaging implications

- **The Camera Extension gates to the official signed build** — the
  `system-extension.install` entitlement + the embedded `.appex` are baked into
  *macrdp's* signature/profile, so a `cargo build` from source can run the **protocol
  side** (Phases 0–2, useful for development) but not **present** the camera without
  the signed app + user approval. Milder than the USB entitlement story (that gates
  the *whole* feature and needed an Apple grant); this only gates the macOS
  presentation half and uses a self-serviceable entitlement.
- The protocol side is cross-platform and unit-testable on Linux CI like the other
  channel work; only the CoreMediaIO/VideoToolbox pieces are `#[cfg(macos)]`.
- Everything else (drives, smart cards, USB, video, audio, clipboard) is unaffected —
  default-off feature flag (`--enable-camera-redirection`), byte-identical when off.

## Recommendation

- **Do Phase 0 first** (advertise `RDCamera_Device_Enumerator`, watch for
  `DEVICE_ADDED_NOTIFICATION` from a real mstsc). It's a day of work, needs no
  entitlement and no macOS camera code, and it de-risks the whole thing: if a modern
  mstsc won't offer its camera to macrdp's enumerator, stop before investing.
- If Phase 0 is green, scope the rest as a **genuine multi-week feature** —
  RDPDR/URBDRC-scale for the protocol + decode, plus the Camera Extension for the
  macOS side. Every part is something macrdp has already shown it can do (DVC channels
  ✓, reliable UDP multitransport + inbound tunnel path ✓, VideoToolbox ✓, presenting a
  redirected device as a real macOS device ✓ for USB/drives/smart cards).
- **Not an OSS-first, but there's a reference implementation.** Unlike URBDRC (where
  macrdp was first), **FreeRDP already implements the server side of MS-RDPECAM**
  (`channels/rdpecam/server/camera_device_enumerator_main.c` +
  `camera_device_main.c`) — it decodes the same PDUs we need. That's a working
  reference for the exact wire handling (and a heads-up: it had an out-of-bounds read
  in the `DeviceName`/`VirtualChannelName` scan before 3.28.0, CVE-2026-57157 — bound
  those reads). macrdp presenting the result as a real *macOS* camera would still be
  novel, but frame the value as "a genuinely useful capability," not a first.
- **Prefer this over any further USB-side effort for webcams.** The URBDRC path is
  proven dead for mstsc cameras; MS-RDPECAM is the only route that can work.

## Why this differs from the USB dead-end

Generic USB redirection had to *present hardware* (a virtual host controller) because
mstsc forwards raw URBs — and then mstsc **doesn't even send the webcam that way**.
Camera redirection instead rides a **question/answer media protocol** (enumerate →
negotiate format → receive samples) straight into a macOS plug-in slot (CoreMediaIO),
exactly like smart cards rode PC/SC and drives rode file ops. That is why it's
tractable where the USB path was not: the right layer already exists on both ends.

## Sources

- **MS-RDPECAM** — *Remote Desktop Protocol: Video Capture Virtual Channel Extension*
  (Microsoft Open Specifications, fully documented + PDF). Channel
  `RDCamera_Device_Enumerator`; enumerator + per-device message sets;
  `CAM_MEDIA_TYPE_DESCRIPTION` formats. **This is the camera protocol to implement.**
  <https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpecam/>
- **MS-RDPEVOR** — *Video Optimized Remoting Virtual Channel Extension* (channels
  `Microsoft::Windows::RDS::Video::Control/Data::v08.01`) — for context / to *avoid
  confusion*: this is **server→client** desktop-video optimization (dependent on the
  Graphics channel), **not** camera redirection. Seen in the capture but a red herring
  for the webcam. <https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpevor/>
- Ground-truth capture `~/mstscpcap_camera3.pcapng` (mstsc → Win10 Pro RDS, decrypted
  2026-07-16): no `URBDRC`; the webcam's 50 MB client→server rode the reliable UDP
  tunnel (no DTLS) as an **MS-RDPECAM** channel *inside* the tunnel — invisible to the
  TCP-side census (which showed only the server→client MS-RDPEVOR `RDS::Video` channels
  above). The tunnel's TLS is RSA-KX (ServerHello `0x009d`), so `rdpkey.pem` could
  decrypt it, but tshark can't chain RDPEUDP→TLS. Decryption method + full DVC census +
  the MS-RDPEVOR correction in the `project_usb_redirection_feasibility` memory.
- Apple: **Creating a camera extension with Core Media I/O** (`CMIOExtension`,
  System Extension packaging + `SystemExtensions.framework` activation).
- Cross-reference: `docs/usb-redirection-feasibility.md` (why the webcam is NOT USB),
  `docs/rdp-udp-multitransport-feasibility.md` (the reliable tunnel + inbound
  DVC path the camera uses), `docs/smart-card-redirection.md` (the device-class-first
  pattern this follows).

## Status

**Not started — scoping only.** No code, no feature flag, no vendored divergence yet.
The prerequisite finding (webcam = MS-RDPECAM-class channel, not USB) is proven; the
recommended first move is the Phase-0 protocol gate above.
