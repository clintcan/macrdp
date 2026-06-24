# Generic USB Redirection on macrdp — Feasibility Notes

*Research notes, 2026-06-20. Exploratory — macrdp does **not** implement generic
USB redirection today, and nothing here is committed work. This is a scoping
document for if/when it's ever pursued.*

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

1. **Request + obtain** `com.apple.developer.usb.host-controller-interface` for the
   macrdp signing team. (Phase-0 spike — cheap, decisive.) A ready-to-paste
   Feedback Assistant draft is in [`docs/entitlement-request.md`](entitlement-request.md).
2. **Provisioning profile.** macrdp currently signs with plain **Developer ID** (no
   profile). A managed entitlement must be embedded in a **provisioning profile**, so
   the build/sign pipeline (`packaging/make-app.sh`) gains a profile step.
3. **Implement `MS-RDPEUSB` server-direction** in vendored IronRDP (URB DVC,
   device announce/remove, transfer types). Large.
4. **Implement the UserHCI virtual controller** (`src/usb/…`): create the controller
   interface, drive the device/endpoint state machines, translate EUSB URBs ↔ the
   controller-interface transfer model. Private SPI; expect a `private_api.rs`-style
   maintenance boundary like `virtual_display/`.
5. **Lifecycle**: device hotplug on client redirect, teardown on disconnect/exit.

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

## Status

**Exploratory / not started.** No code, no entitlement requested. Cross-reference:
the `docs/known-quirks.md` smart-card note (kext vs dext vs UserHCI rationale).
