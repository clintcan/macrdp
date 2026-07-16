# Generic USB Redirection on macrdp — Feasibility Notes

*Research notes, 2026-06-20. Exploratory — macrdp does **not** implement generic
USB redirection today, and nothing here is committed work. This is a scoping
document for if/when it's ever pursued.*

> **UPDATE 2026-07-01 — Phase 1 is GO ✅, and two early assumptions below were wrong.**
> The entitlement `com.apple.developer.usb.host-controller-interface` was **granted** to
> team QGLA89KHM7 (FB23363880). A signed+provisioned spike (`--usb-spike`, `src/usb_redirect/`)
> successfully instantiated `IOUSBHostControllerInterface` and the kernel driver began the
> command exchange — so the entitlement functions and the UserHCI route is real.
> **Correction 1:** `IOUSBHostControllerInterface` is **NOT undocumented private SPI** — it's a
> **public, SDK-headered API** in the public `IOUSBHost.framework` (headers incl.
> `IOUSBHostControllerInterface.h` + the `IOUSBHostCI*StateMachine.h` set) with a complete
> example `main()`; its doc literally says it "create[s] synthetic USB devices." So the "private
> SPI, hard, moving target" and "VirtualHere binary forensics" framing below is superseded —
> it's documented-API FFI. **Correction 2:** upstream IronRDP now has an `ironrdp-rdpeusb` crate
> with the **complete bidirectional MS-RDPEUSB PDU layer** (client processor only), so the
> protocol side is no longer "unprecedented / from scratch" — we add a server processor on that.
> See the `project_usb_redirection_feasibility` memory for specifics.
>
> **UPDATE 2026-07-06 — Phases 2 and 3.0 are GO ✅ (branch `feat/usb-redirect-spike`).**
> - **Phase 2 (P2 below) DONE** (commit `ab91a63`): `src/usb_redirect/usb_spike.m` drives the
>   full UserHCI command/doorbell loop and a **hardcoded synthetic device enumerates LIVE in
>   `ioreg`** (VID 0x1209/PID 0x0001, complete EP0 GET_DESCRIPTOR flow, clean teardown) — the
>   whole macOS *presenting* path is proven end-to-end.
> - **Phase 3.0 (a go/no-go slice of P3) DONE** (commit `3a435c9`): a server-direction
>   `URBDRC` DVC **observe-only** processor (`--enable-usb-redirection`) advertises the channel
>   and runs the MS-RDPEUSB capability exchange. **Verified GREEN locally with a plain
>   `cargo build`** — the observe-only slice never touches the UserHCI controller, so **no
>   entitled build is needed for it** — via `sdl-freerdp /usb:auto`: the client opens URBDRC
>   (Create status 0) and completes the caps exchange (S_OK). (No `AddDevice` only because the
>   test Mac had no attachable USB device to redirect; channel-open + caps-exchange is the gate.)
>   Built as vendored `ironrdp-server` **divergence 16** (`src/rdpeusb.rs`), with
>   `ironrdp-rdpeusb` pulled in as a pinned-rev git dep (PDU-only — we drive the wire ourselves,
>   the same pattern as the server-direction RDPDR, rather than bump the whole IronRDP pin).
> - **Remaining: Phase 3.1** — the real forward: grow the processor into the handshake state
>   machine + an async `UsbHandle`/`UsbRouter` transfer path, and evolve `usb_spike.m` from
>   hardcoded+synchronous to client-sourced+async (the IOUSBHost-serial-queue ↔ tokio boundary).
>   Live 3.1 verification needs a physical redirectable device (or mstsc + the RemoteFX-USB
>   Group Policy). The *presenting* side still gates to the signed+provisioned entitled build.
>
> **UPDATE 2026-07-08 — HID-input (gamepad) VERIFIED WORKING ✅✅, first HID-class device, NO server change.**
> A client-redirected **Xbox controller** (`045e:0b12` — vendor-class GIP, `bInterfaceClass=0xFF`,
> not plain HID) over Linux FreeRDP is a **live, button-responsive gamepad on the Mac**:
> macOS binds its own `XboxSeriesXGamepad` / `com.apple.gamecontroller.driver.XboxGamepad`
> driver, and 2600+ interrupt-IN input reports (`moved=44` each) stream through and render in
> gamepad-tester — **cold-start included** (macOS's own GIP power-on over the redirect wakes a
> never-initialized controller; it sends the 5-byte `05 20 00 01 00` power-on **and** 13-byte
> follow-up init on `ep=0x02`). The **interrupt-transfer path already carried this** — `raiseBulk`/
> `completeTransferToken` in `usb_spike.m` handle interrupt endpoints as bulk (they share the
> `TS_URB_BULK_OR_INTERRUPT_TRANSFER` URB), so **no code change was needed**. The one requirement
> is the **standard USB-redirection client setup**: release the client's own driver so the redirect
> can claim the interface (`sudo modprobe -r xpad` on Linux; a first attempt that showed empty
> reads was purely `LIBUSB_ERROR_ACCESS` — xpad holding the interface — not a server gap). Two
> minor items surfaced, neither blocking: (1) a **cold controller must have a claimable interface**
> (client-setup, above); (2) `IOUSBHostCIMessageTypeLink` (`0x3c`) transfer-ring descriptors on
> EP0 are dropped-and-halt the ring walk instead of being followed — a latent correctness bug on
> EP0's ring (input rides `ep=0x82`'s own ring, so unaffected), worth a small hardening fix.
> Other device classes (audio, printers, …) remain untested.

## TL;DR

- **macrdp could, in principle, do generic USB redirection**, and the right macOS
  mechanism is a **user-space virtual USB host controller** — `IOUSBHostControllerInterface`
  driving Apple's built-in `AppleUSBUserHCI` provider ("UserHCI"). This is the same
  trick VirtualHere's client appears to use (per binary forensics — see below).
- The capability is **gated by a managed entitlement, `com.apple.developer.usb.host-controller-interface`**,
  which is **granted on request** (Feedback Assistant + Team ID) and is **not** the
  contentious DriverKit `transport.usb` entitlement that stalls `usbipd-mac`. So
  the permission is a *hurdle, not a hard wall*.
- The **real cost is the two big builds**: the RDP protocol side (`MS-RDPEUSB`,
  URB-level USB redirection — not in IronRDP, and server-side EUSB is essentially
  unprecedented) and the **UserHCI virtual-controller** implementation (private SPI,
  hard, a moving target).
- **Recommendation:** treat it as a large research project gated on a Phase-0
  entitlement-request spike. For most concrete needs, **device-class redirection**
  (drives ✅, smart cards ✅, printers/scanners/audio possible at the protocol
  layer) is dramatically less work and risk than generic USB.

## Context — what macrdp does today

macrdp redirects devices by **device class / protocol**, never by presenting raw
USB hardware:

- **Drive redirection** (RDPDR / MS-RDPEFS) → a real NFS mount.
- **Smart-card redirection** (RDPDR / MS-RDPESC) → a user-space PC/SC IFD handler.

Both ride a *protocol* (file ops, PC/SC APDUs) that macOS already exposes a
plug-in point for, so there is no virtual hardware to synthesize. **Generic USB
redirection is a different beast**: the client redirects an *arbitrary* USB device
(URB level), and the server has to make it appear as a real local USB device so any
macOS driver/app binds to it.

## The two layers any solution needs

In RDP, the **client** redirects its USB device to the **server** — so macrdp would
be the **presenting/consuming** side (the harder side, in VirtualHere terms).

1. **RDP protocol layer** — receive the redirected device + its USB traffic.
2. **macOS presentation layer** — present it as a local USB device and pump the
   traffic through.

## Layer 1 — the RDP protocol (`MS-RDPEUSB`)

RDP's generic USB redirection is **MS-RDPEUSB** (RemoteFX USB redirection): raw
**URB** (USB Request Block) forwarding over a **dynamic virtual channel** (DVC),
with device add/remove, channel setup, isoch/bulk/interrupt/control transfers, etc.

- **Server-direction is not in IronRDP** — this would be substantial vendoring,
  bigger than the RDPDR / RDPESC work already done. (Update 2026-06-25: IronRDP is
  adding *client*-side RDPEUSB — see [issue #1140 "[ironrdp-client] Wire up
  RDPEUSB with libusb backend"](https://github.com/Devolutions/IronRDP/issues/1140).
  That's the consuming/client direction; macrdp would still need the *server*
  direction, which remains absent. The client work could still be a useful PDU /
  URB-codec reference if it lands.)
- **Server-side EUSB is essentially unprecedented.** FreeRDP implements the *client*
  side (`urbdrc`); the server side is normally just Windows' own USB stack. macrdp
  would be charting new ground.

## Layer 2 — presenting the device on macOS (three options)

| Option | Mechanism | Status / cost |
|---|---|---|
| **Kext** | A kernel extension creating a virtual USB host controller (VirtualHere's *old* `vhhcd.kext`) | **Dead.** Deprecated; needs kext-signing or SIP off + reboot + user approval. Don't. |
| **DriverKit dext** | `USBDriverKit` System Extension | Needs the **restricted `com.apple.developer.driverkit.transport.usb`** entitlement — the wall **`usbipd-mac`** has been unable to get past. Heavy (System Extension, host app, approval). |
| **User-space virtual HCI** ⭐ | `IOUSBHostControllerInterface` → Apple's built-in `AppleUSBUserHCI` provider | **The viable route.** No third-party kext/dext. Needs the **grantable `com.apple.developer.usb.host-controller-interface`** entitlement. Private SPI, hard, moving target. |

### Why "user-space virtual HCI" is the one to use

A user-space process implements a USB **host controller interface** with
`IOUSBHostControllerInterface` + the `IOUSBHostCI*StateMachine` classes; it attaches
to Apple's **own** built-in `AppleUSBUserHCI` kernel provider, which registers the
synthetic device in the IORegistry so normal macOS drivers bind to it. The process
then feeds the controller with devices/endpoints backed by the remote (redirected)
device.

Important nuance: **it's "no *third-party* driver," not "no driver at all."** The
device is still registered by Apple's kernel HCI provider — it's just *driven from
user space*. That's the win: nothing to kext-sign, no dext entitlement gauntlet,
no SIP changes.

### Evidence VirtualHere takes this route

Binary forensics of VirtualHere's macOS client (shared via a reader, `nm`/`strings`):

- `IOUSBHostControllerInterface`, `IOUSBHostCIDeviceStateMachine`,
  `IOUSBHostCIEndpointStateMachine`, `IOUSBHostCIMessageTypeToString`
- `AppleUSBUserHCI…` / `…erHCIUserClient`
- `OSX11ClientDriver.mm`, `handleDeviceBind`/`Unbind`, "creating Host Controller",
  "virtual ports", `IOUSBHostDevice`

Those are the symbols of something *implementing* a host controller, not merely
*talking to* USB devices. (Inference from symbols, not confirmed runtime tracing —
the decisive confirmation would be a `ioreg -p IOUSB` before/after diff while a
redirected device is in use.)

## The entitlement situation (the make-or-break question)

- The UserHCI path requires the **managed** entitlement
  **`com.apple.developer.usb.host-controller-interface`**.
- It is **not** in the Xcode Capabilities panel (low request volume, never
  integrated into the portal), so it **can't be self-added** — Xcode will reject it
  if it isn't already in your provisioning profile.
- **You request it from Apple** via Feedback Assistant (macOS → Problem area "USB"),
  including your **Team ID**, a product overview, and any marketing links; per Apple
  forum guidance the approving engineer "is generally good about handling these
  quickly." No SIP-disable or root is mentioned.
- **This is a different, easier entitlement than the DriverKit one.** `usbipd-mac`'s
  blocker is `com.apple.developer.driverkit.transport.usb`; the UserHCI route needs
  `com.apple.developer.usb.host-controller-interface`, which appears genuinely
  obtainable. They are not the same wall.

## What it would take for macrdp (concretely)

*(Status tags added 2026-07-06; the two UPDATE blocks at the top of this file carry
the current picture — this list is the original scoping.)*

1. **[DONE ✅]** **Request + obtain** `com.apple.developer.usb.host-controller-interface`
   for the macrdp signing team. Granted to QGLA89KHM7 (FB23363880). The
   Feedback Assistant draft used is in [`docs/entitlement-request.md`](entitlement-request.md).
2. **[DONE ✅]** **Provisioning profile.** `packaging/make-app.sh` gained the profile step
   (`PROVISION_PROFILE=…`); the profile embedding the USB capability lives OUTSIDE the repo
   at `../provcerts/macrdp/macrdpprov2.provisionprofile` (a secret — never committed).
3. **[IN PROGRESS]** **Implement `MS-RDPEUSB` server-direction** in vendored IronRDP (URB DVC,
   device announce/remove, transfer types). **Phases 3.0 + 3.1a done** (divergence 16): the
   init handshake, the per-device DVC (opened via `ServerEvent::Urbdrc` →
   `DrdynvcServer::create_channel`), and the client's real `ADD_DEVICE` are all verified live.
   Remaining: **3.1b** — extend the caps decoder to parse USB-3 descriptors (vendor
   `ironrdp-rdpeusb`) + the async transfer path. Note: we write our own `UrbdrcServer` on the
   pinned `ironrdp-rdpeusb` **PDU layer** rather than adopt upstream's newer processor (which
   needs a breaking IronRDP pin bump).
4. **[IN PROGRESS — Phase 2 done]** **Implement the UserHCI virtual controller**
   (`src/usb_redirect/usb_spike.m`): **Phase 2 done** — a hardcoded synthetic device
   enumerates in `ioreg`. *(Correction: this is public IOUSBHost.framework API, NOT private
   SPI — see the top-of-file corrections; no `private_api.rs`-style boundary needed.)* Phase 3.1
   swaps the hardcoded descriptors + synchronous transfers for the client's real device over
   URBDRC (the async IOUSBHost-queue ↔ tokio boundary).
5. **[Phase 3.1+]** **Lifecycle**: device hotplug on client redirect, teardown on disconnect/exit.

## Distribution / packaging implications

- **Gated to the official signed build.** The entitlement is baked into *macrdp's*
  signature/profile. Ad-hoc CI artifacts and users building from source **could not**
  use USB redirection — only the build signed with the macrdp team's profile could.
  This breaks macrdp's "anyone can `cargo build` it and get every feature" property
  for this one feature (analogous in spirit to the smart-card USB-trigger caveat, but
  stronger — it gates the whole feature, not just deployment).
- Everything else (drives, smart cards, video, audio, clipboard) is unaffected.

## Recommendation

- **Gate everything on the entitlement spike first** (request it; cheap, and it's
  reportedly fast). If Apple declines for an RDP-server use case, stop — it's a hard
  wall and not worth the protocol work.
- If granted, scope the **MS-RDPEUSB + UserHCI** build as a genuine multi-week
  research project, with the UserHCI piece behind a private-API maintenance boundary.
- **Prefer device-class redirection when a specific class covers the need.** Generic
  USB only pays off for *arbitrary/custom* hardware nothing else can carry. Printers,
  scanners, audio, etc. each have their own RDP/protocol path that's far cheaper and
  carries no entitlement/SPI risk — the same "device-class beats generic USB" logic
  that made smart cards a user-space PC/SC handler instead of raw USB.

## Why this mirrors the smart-card decision

Smart cards rode a **question/answer protocol** (PC/SC) straight into a plug-in slot
macOS already exposes — no virtual hardware, no entitlement, no SPI. Generic USB has
no such slot, so it forces you down to *presenting hardware* — which is precisely the
heavy `IOUSBHostControllerInterface`/UserHCI + entitlement + protocol path above.
That contrast is the whole reason macrdp's redirection strategy is device-class-first.

## Sources

- Gist — creating virtual USB devices via `IOUSBHostControllerInterface` (states the
  required entitlement): <https://gist.github.com/JJTech0130/fae6b6ee6ae4232172a9188fb199d5d9>
- Apple Developer Forums — "is `com.apple.developer.usb.host-controller-interface`
  managed?" (how to request it): <https://developer.apple.com/forums/thread/802495>
- Apple Developer Forums — DriverKit `transport.usb` entitlement friction (the
  contrasting wall): <https://developer.apple.com/forums/thread/708501>
- `objc2-io-usb-host` crate (Rust bindings exist for the CI classes):
  <https://docs.rs/objc2-io-usb-host>
- VirtualHere (product): <https://www.virtualhere.com/>
- `usbipd-mac` (USB/IP for macOS, blocked on the DriverKit USB entitlement):
  <https://github.com/beriberikix/usbipd-mac>

## First open-source RDP *server* to present a redirected USB device

As far as is known, macrdp is the **first (and currently only) open-source RDP server
that receives a client-redirected USB device and presents it as a real local device** —
i.e. it implements the **server direction** of MS-RDPEUSB (`URBDRC`) plus local device
synthesis. This mirrors the project's earlier UDP-multitransport finding (first OSS RDP
server with a working UDP data path).

The claim is specifically about the *server/presenting* side. USB redirection in RDP is
inherently **client → server**: the client owns the physical device and redirects it; the
server must synthesize/present it. On real Windows RDS that presentation is done by
**closed-source kernel drivers** (`usbdr.sys` / the RemoteFX USB bus), not by any OSS
server.

**Verified 2026-07-06 against current sources:**
- **FreeRDP** — the most complete OSS RDP stack — implements `URBDRC` **client-direction
  only**. Its `channels/urbdrc/` tree has `client/` and `common/` subdirectories and **no
  `server/`** (`common/` is just the shared `msusb.c` PDU marshaling). FreeRDP issue
  [#7558 "server side channel not implemented"](https://github.com/FreeRDP/FreeRDP/issues/7558)
  documents this, and the project's own guidance states "the urbdrc channel has only the
  client side implemented." So FreeRDP-based servers (ogon, freerdp-shadow) cannot present a
  redirected device.
- **xrdp**, **ogon**, **gnome-remote-desktop** — no USB-redirection code at all (source-tree
  greps for `urbdrc`/`usbredir`/`usb_redir` returned nothing).
- **VirtualHere / usbip** present remote USB devices, but they are **USB-over-IP** (their own
  protocols), not RDP — and VirtualHere is proprietary.

Scope/hedge: "as far as is known" — this is a negative-existence claim over the OSS
landscape; it's backed by the source checks above, not a proof no niche project exists.
macrdp's presenting side is macOS-only (the UserHCI virtual host controller) and needs the
entitled/provisioned build.

## Status

**In progress — Phases 1, 2, 3.0, 3.1, and 3.2 (bulk/mount) done** (branch `feat/usb-redirect-spike`).
The entitlement `com.apple.developer.usb.host-controller-interface` is **granted**
(team QGLA89KHM7, FB23363880).
- **Phase 1 GO** — entitled build instantiates the `IOUSBHostControllerInterface`
  controller; kernel command exchange begins.
- **Phase 2 GO** — a hardcoded synthetic device enumerates live in `ioreg` (the whole
  macOS UserHCI presenting path proven).
- **Phase 3.0 GO** — the server-direction `URBDRC` DVC + MS-RDPEUSB init handshake
  (caps → CHANNEL_CREATED → RIMCALL_RELEASE) drives a real client to announce a device
  (`ADD_VIRTUAL_CHANNEL`), verified with a purpose-built FreeRDP-with-urbdrc client.
- **Phase 3.1a GO** — the server opens a **per-device DVC** on demand
  (`ServerEvent::Urbdrc` → `DrdynvcServer::create_channel`) and the client's real
  `ADD_DEVICE` (device descriptors) arrives on it (verified live, USB-3 flash drive).
  Both DVC `process()` impls tolerate decode errors so an unparseable PDU never tears
  down the session. Vendored `ironrdp-server` divergence 16.
- **Phase 3.1b(1) GO** — `ADD_DEVICE` now **fully parses** (real descriptors). The
  pinned `ironrdp-rdpeusb` `SupportedUsbVer` enum stopped at USB 2.0 and rejected a
  modern device's `0x320` (USB 3.2) caps, so `ironrdp-rdpeusb` is now **vendored** with
  a lenient `UsbDeviceCaps` decode (USB 3.x versions + `Other(u32)` fallbacks). Verified
  live with a USB-3.2 flash drive (`usb_version=Usb32`). See
  `vendor/ironrdp-rdpeusb/CLAUDE.md`.
- **Phase 3.1b(2a) GO** — a server-initiated **`GET_DESCRIPTOR` control transfer**
  round-trips real device data (proven observe-only, plain `cargo build`): on
  `ADD_DEVICE` the device processor sends `RegisterRequestCallback` +
  `TransferInRequest` and decodes the `URB_COMPLETION`. Verified live with a USB-3.2
  flash drive (`vid=0x2174 pid=0x2100`, read from the physical device). This de-risks
  the transfer path — libusb kernel-detach was not a blocker after unmount.
- **Phase 3.1b(2b) GO ✅✅ — a real client device enumerates locally** — the transfer
  path became a reusable async `UsbHandle`/`UsbRouter`, the driver moved into macrdp
  (`src/usb_redirect/mod.rs::drive_device`) via a `device_callback` seam, and
  `usb_spike.m` was restructured to async out-of-band EP0 completion. Verified entitled +
  FreeRDP: the client-redirected ESD310C flash drive enumerates on macrdp's UserHCI
  controller with descriptors/strings sourced live from the client. Controller is
  destroyed on disconnect (a `watch` channel → `closed()`), not leaked.
- **Phase 3.2 GO ✅✅ — the redirected USB DRIVE MOUNTS on the Mac** — `select_configuration`
  opens the device's pipe handles and `UsbHandle::bulk_transfer_in/out`
  (`TsUrb::BulkInterruptTransfer`) forward bulk on the mass-storage endpoints, so the
  macOS driver's SCSI (CBW/data/CSW) rides the client's real drive. **Verified end-to-end
  on a real Linux FreeRDP client** (UTM-QEMU Ubuntu + a USB-2.0 hub for a claimable
  interface): the ESD310C **mounts and stays mounted** (1300+ steady bulk transfers, no
  resets/timeouts). Two load-bearing fixes: dedup on the device's hardware identity
  (`VID:PID:bcdDevice`, not the client's per-announce instance id — FreeRDP double-announces
  one drive, and presenting both duels two virtual drives over the one device); and an
  Obj-C endpoint-object identity guard on completion (a device reset destroys+recreates the
  endpoint at the same key, leaving a pending transfer pointing into a freed ring). EP0
  **control-OUT** forwarding also lands (mass-storage Bulk-Only Reset / Clear-Feature ride
  `UsbHandle::control_transfer_out`, a generic `CONTROL_TRANSFER_EX`; SET_ADDRESS/CONFIGURATION/
  INTERFACE stay local ACKs), regression-verified but only fires under a SCSI error.

- **Hardening pass (2026-07-07)** — five review fixes, all live-verified with the
  connect-while-mounted repro (the drive now mounts and stays, no crash): (1) a
  **disconnect-race deadlock** — every transfer awaited a oneshot the handle's own
  `Arc` kept alive, so a disconnect mid-transfer pended forever and leaked the
  controller + dedup slot; `UsbHandle::await_reply` now races every completion against
  `closed()`. (2) **Generic control-IN forwarding** — `UsbHandle::control_transfer_in`
  forwards any non-standard-descriptor EP0 IN with the raw SETUP preserved, so
  mass-storage **Get Max LUN** (multi-LUN) and HID report-descriptor reads work
  (verified forwarded+answered live). (3) **Per-endpoint transfer supersession** — a
  slow device missing the kernel's ~5 s EP0 timeout let the kernel re-issue the ring
  slot while the original completion was still in flight → SIGBUS through the retired
  slot; the Obj-C side now invalidates a superseded transfer at raise time (this was
  the connect-while-mounted crash). (4) **Client channel-close → teardown** — a
  one-line `ironrdp-dvc` divergence now invokes the (previously-dead) `DvcProcessor::close`
  hook, so the client closing a per-device channel (device unplug/reset) tears the
  controller down and releases the dedup slot → hot-unplug + reset re-present
  (**verified live 2026-07-07**: detach in UTM → controller torn down → re-attach
  re-mounts). (5) the Obj-C OUT data-stage reports the full accepted length.
  **"Disk not ejected properly" on client-stop is expected/correct** — the client
  vanished, macrdp destroys the virtual controller (verified to cleanly remove the
  device from `ioreg`), and macOS reports the ungraceful removal exactly like yanking a
  USB stick. Remaining 3.2: an explicit RETRACT_DEVICE PDU (channel-close covers the
  common case), true multi-device (iSerialNumber), non-mass-storage device classes.

- **mstsc client — ENUMERATES end-to-end (2026-07-07), does not STREAM yet.** All
  prior verification was on FreeRDP; mstsc (RemoteFX USB) is stricter and needed three
  interop fixes before a device would even announce. Diagnosed with a silence-vs-message
  A/B on the per-device channel: mstsc keeps a *silent* per-device channel open and
  waits, but CLOSES it ~4 ms after any out-of-sequence first message. The fixes (all
  FreeRDP-safe, merged to `main`):
  1. **Per-device channel needs the full handshake** — capability exchange →
     `CHANNEL_CREATED` → `RIMCALL_RELEASE`, exactly like the main channel, not just
     `RIMCALL_RELEASE`. This was THE blocker (mstsc sent a DVC Close, never `ADD_DEVICE`);
     FreeRDP tolerated the shortcut because its readiness state is global to the main
     channel. Vendored `ironrdp-server` divergence 16.
  2. **Accept `UsbDevice = 0`** — mstsc assigns interface-id 0 to a redirected device
     (it's disambiguated by its own DVC); the decoder rejected the reserved `0x0..=0x3`
     range, so every mstsc `ADD_DEVICE` failed to parse. Vendored `ironrdp-rdpeusb`
     divergence 1.
  3. **Route interface-0 URB completions by function id** — a consequence of (2): the
     `GET_DESCRIPTOR` completion arrives on interface 0 (== CAPABILITIES) and was
     mis-decoded as a capability response and silently dropped. Vendored `ironrdp-rdpeusb`
     divergence 2.
  With all three, a real mstsc device handshakes, announces `ADD_DEVICE`, and macrdp
  fetches its descriptors + presents the UserHCI controller — **verified live with an
  A4Tech camera (`09da:2692`) and a USB Audio/HID device (`0573:1573`)**.

- **mstsc — CONFIGURES + negotiates format end-to-end (2026-07-07); only the bulk
  frame delivery is missing (client-side).** Four more fixes carried mstsc past
  enumeration all the way to a streaming attempt, each another mstsc-strict /
  FreeRDP-lenient item, all regression-checked against FreeRDP mass storage (drive still
  mounts + read/write — the macOS "not readable" dialogs are the drive's own Linux
  partitions, not us):
  1. **`SelectConfiguration` (`0x80070057` → succeeds).** Two bugs, neither tripped by
     mass storage (one interface, no alt settings): (a) `parse_configuration` emitted one
     interface-info per interface *descriptor* — including every alternate setting —
     producing **duplicate interface numbers**, which real Windows rejects; it now emits
     one entry per interface **number** at alternate setting 0. (b) the URB carried only
     the 9-byte config-descriptor header while `ConfigurationDescriptorIsValid` was set;
     real Windows walks `wTotalLength` and rejects a truncated descriptor — fixed by
     carrying the **full** configuration descriptor (`ironrdp-rdpeusb` divergence 3).
  2. **Control transfers (`0x80070057` → succeed, 135+).** mstsc rejects the generic
     `URB_FUNCTION_CONTROL_TRANSFER_EX`; `setup_to_typed_urb` maps each SETUP packet to
     the specific typed URB real Windows emits (`CLASS_INTERFACE`,
     `GET_DESCRIPTOR_FROM_INTERFACE`, `SET_FEATURE_TO_*`, …), keeping `CONTROL_TRANSFER_EX`
     as a fallback. The **UVC VS_PROBE/COMMIT format negotiation completes**.
  3. **`USBD_SHORT_TRANSFER_OK`** on bulk/interrupt IN (a short read — a video payload —
     is normal, not `0x8007001f`). A no-op for mass storage's exact-length SCSI reads.
  4. **`RIMCALL_RELEASE` recognized + ignored** (mstsc sends one per completed request to
     release the callback we registered; investigated as a suspected dropped-frame path,
     confirmed benign).
  With a camera opened in Photo Booth on the Mac, the format negotiates and macOS issues
  continuous bulk reads on the video endpoint — but **mstsc never returns frame data** (of
  ~10 concurrent bulk reads it completes one with `0x8007001f` and leaves the rest pending
  forever). This is a **client/mstsc-side limitation, not a server bug:** for a webcam,
  Windows routes real video over the **dedicated camera-redirection channel** ("Video
  capture devices" in mstsc), a separate high-level protocol macrdp doesn't implement —
  true webcam support is that channel, a distinct future feature. Note mstsc's RemoteFX
  USB list **excludes mass storage** (it rides *Drives*/RDPDR), so the verified bulk/mount
  path can't be exercised from mstsc; isochronous (camera/audio) endpoints remain
  unimplemented (interrupt/HID is now done — the gamepad).

**Bulk webcam over FreeRDP = STREAMS live video (2026-07-08) — the URB-depth read-ahead
engine (divergence-16 refinement in `usb_spike.m`).** The same A4Tech camera that only
enumerated over mstsc **streams smooth moving video into Photo Booth over FreeRDP** (its
VideoStreaming EP 0x82 is **bulk**, not isochronous). The blocker was **URB-depth
starvation**, not the mstsc camera-channel wall: macOS double/triple-buffers a streaming
bulk-IN endpoint (it queues ~3 concurrent reads so the pipe never runs dry), but the
UserHCI ring exposes only ONE transfer (the head) at a time (`currentTransferMessage`
advances only on completion — no peek-ahead). The original code re-forwarded the *same*
head on each re-doorbell → the device got depth but half the completions were dropped (a
bulk-IN pipe is a stream) → corrupt/no picture; collapsing that to strictly one-read-at-a-
time (a de-dup skip-guard) → the device *underran* (`moved=0`, macOS destroyed the endpoint
after ~7 s and looped COMMIT→read→timeout→teardown forever). **Fix = a bulk-IN read-ahead
engine:** on the re-queued-head signal AND a large read (`readLen >= 512`, so 16-byte
interrupt polls stay serial), keep `MACRDP_USB_PREFETCH_DEPTH` (default 4; `1` disables)
concurrent `bulk_transfer_in` reads in flight to the client — **decoupled from the ring** —
buffered in **sequence order** (`MacrdpPrefetchRead.seq` + a reorder map, so out-of-order
client completions can't scramble frames) and handed one chunk per ring TRB as it becomes
the head. Restores device URB depth with **no data loss**; same UAF guards on every `msg`
write; `tearDownStream` on EndpointDestroy. **Gated so mass storage (serial BOT — never
re-queues) and interrupt/gamepad (single-outstanding) never engage** — regression-verified
live: the flash drive still mounts + sha256 byte-exact with **no** `read-ahead engaged` line
for its endpoint. No Rust change (the URBDRC/`UsbHandle`/`UsbRouter` side is already
per-token concurrent; the completion FFI carries only a token). Log marker:
`bulk-IN read-ahead engaged ep=0x82 depth=4 readLen=102656`. Remaining: **isochronous**
webcams (a different transport, not yet built) and the mstsc camera-redirection channel.

**Merge-readiness (branch `feat/usb-redirect-spike`).** The feature is fully wired and
opt-in: `--enable-usb-redirection` (default OFF; when off the URBDRC factory is `None`, so
the build is inert), `ENABLE_USB_REDIRECTION` in `config.env`, `docs/cli.md` + `--help`
documented. CI green;
`cargo clippy`/`test`/`fmt` clean. It's **safe to merge as EXPERIMENTAL** (the UDP-multitransport
precedent) — the robustness gaps found in review (disconnect-race deadlock, the connect-while-
mounted SIGBUS, control forwarding, hot-unplug) are now closed and the clean path is
field-verified. What's genuinely deferred before it's a *supported* feature: an explicit
RETRACT_DEVICE PDU, true multi-device, and verification of device classes beyond mass storage.
Presenting side is macOS-only and needs the entitled/provisioned build.

Cross-reference the `docs/known-quirks.md` smart-card note (kext vs dext vs UserHCI
rationale) and `project_usb_redirection_feasibility` memory for the running log.

## 2026-07-16 — webcam verification: FreeRDP GREEN on v0.8.35, mstsc refusal packet-proven

A full-day webcam-redirect verification pass against the shipped **v0.8.35** entitled build
produced two durable findings.

- **FreeRDP: webcam works end-to-end — regression PASS.** The A4Tech FHD 1080P
  (`09da:2692`, bulk UVC) redirected from a FreeRDP client streams live video into Photo
  Booth on the Mac. macrdp's URBDRC path is intact: it dedups FreeRDP's double-announce
  (`…d33`/`…d32` → one UserHCI controller, `skipping duplicate identity=09da:2692:0100`),
  engages read-ahead (`ep=0x82 depth=4 readLen=102656`), forwards the video reads, and
  returns **zero `0x8007001f`**. **Testing-rig gotcha (cost a day to isolate):** a UVC
  webcam *cannot* be passed through to a UTM/QEMU guest running on the **same** Mac —
  macOS's DriverKit camera stack (`com.apple.cmio.videodriverkithostextension` /
  `UVCAssistant`) binds the camera and there is **no macOS equivalent of Linux
  `modprobe -r uvcvideo`**, so the host keeps the cam (`ioreg` shows it `matched` even with
  the client stopped), the guest's raw-USB reads complete `moved=0`, and the redirect
  starves → black Photo Booth. Tell-tale: two identical A4Tech entries in Photo Booth (the
  physical cam the host still holds + macrdp's redirect), one black. **Fix: run the FreeRDP
  client on a SEPARATE physical machine** — Linux `modprobe -r uvcvideo` + `/usb:dev:09da:2692`,
  or Windows Zadig→WinUSB + `/usb:` — verified working from another machine's UTM the same
  day. Mass storage + gamepad pass through the same-Mac rig fine (macOS doesn't lock those).
  A `build-freerdp-urbdrc.sh` helper (FreeRDP 3.x, `WITH_URBDRC=ON` + `libusb-1.0-0-dev`,
  +FFmpeg H.264/AAC) stands up a Debian client.

- **mstsc: webcam-over-URBDRC is NOT server-fixable — proven at the packet level.** A
  decrypted TCP capture of mstsc→macrdp (`SSLKEYLOGFILE`, TLS 1.3; tshark
  `-o tls.keylog_file:… -d tcp.port==3390,tpkt -d tls.port==3390,tpkt` — the second decode-as
  routes decrypted payload back through TPKT→RDP so `rdp_drdynvc` dissects) shows URBDRC on
  DRDYNVC channel `0x05`: macrdp requests the `102656`-byte (`0x19100`) bulk reads on ep 0x82,
  and mstsc answers **35 completions carrying hresult `0x8007001f` with zero transferred bytes**
  (`…08001b00 040000c0 1f000780 00000000`). The largest single client→server payload is **795 B**
  (a config descriptor) with **no DATA_FIRST fragmentation anywhere** — i.e. mstsc ships no webcam
  data at all, while the desktop H.264 (11 MB) flowed on the same TLS connection throughout. So
  it's a client-side refusal, not a macrdp drop/encode/transport issue. Identity spoofing (RDP
  version, OS platform), RDCamera-channel presence, and TCP-only were all separately ruled out.
  mstsc routes webcams over its own **MS-RDPECAM** camera channel (Phase-0 protocol gate landed —
  see the camera-redirection feasibility doc), not raw URBDRC — **do not re-chase this.**
