# Multi-monitor sessions: design

Status: **design, not started** (2026-11-25). Replaces the June 2026 exploration, which stopped at the
acceptor's single-monitor layout; most of that blocker has since gone upstream.

## Goal

A client that uses several monitors (mstsc "Use all my monitors", Windows App's multi-display option)
gets one remote monitor per local one: each its own full-screen, snapping and maximise target, at its own
resolution and position.

On the Mac, each client monitor is a **virtual display** (`--virtual-display`), so the Mac itself has that
many monitors: windows, full-screen apps and Spaces work per monitor. The Mac's physical panels are not
captured as extra monitors.

## Non-goals (for now)

- Capturing several *physical* Mac displays as separate monitors.
- More than the H.264 encoder can carry: the hardware encoder is shared, so two 1080p60 monitors is the
  realistic ceiling (measured earlier: two VideoToolbox sessions don't run in parallel). Larger layouts
  work at a lower frame rate or bitrate.
- A different scale per monitor in phase 1 (one Retina and one 1x monitor).

## What exists today

- **The client's monitors reach the server.** The acceptor parses the GCC Client Monitor Data and the
  server hands it to `ConnectionHandler::on_connection_info` as `ConnectionInfo::client_monitors`
  (upstream, in the current pin). Mid-session layouts arrive through `RdpServerDisplay::request_layout`
  (MS-RDPEDISP), and `RdpServerDisplay::monitor_count()` (default 1) sets how many the server accepts.
- **Still single-monitor:**
  - The acceptor's `MonitorLayoutSend` state always sends one primary monitor covering the whole
    desktop (`crates/ironrdp-acceptor/src/connection.rs`).
  - `capture.rs::request_layout` uses only the primary monitor from a layout.
  - `h264.rs::setup_locked` sends one monitor in `RESET_GRAPHICS` and creates one surface at (0,0).
  - `CaptureDisplay` holds one display id, and one ScreenCaptureKit stream.
- **Reusable:**
  - `VirtualDisplay` can be created more than once per process; each needs its own serial.
  - EGFX allows several surfaces, each mapped to an output position (`create_surface_with_format`,
    `map_surface_to_output(id, x, y)`), and `resize_with_monitors` (up to 16).
  - Input already maps client coordinates onto a display's live bounds (`input.rs::move_to`).

## Design

### 1. Monitor layout at login (fork divergence)

The acceptor sends the client's own monitors in the Monitor Layout PDU when the session adopted the
client's layout, instead of one monitor covering the desktop. A builder option keeps today's behaviour as
the default, so other IronRDP servers are unaffected. Upstream candidate.

### 2. One virtual display per monitor

At connect, macrdp creates or re-modes one virtual display per client monitor, at that monitor's size,
and arranges them with `CGConfigureDisplayOrigin` to mirror the client's layout (the primary stays the
primary). Displays are reused across connections; extra ones are released when a client brings fewer
monitors.

Each display needs its own identity: the serial is the pid today, so displays get pid-based serials with
an index. `pick_display_id` (#231) and the HiDPI registry (one display today) become per display.

### 3. Capture and video: one surface per monitor

- One ScreenCaptureKit stream per display.
- **H.264:** one EGFX surface and one encoder session per monitor, each surface mapped at its monitor's
  position in the output; `RESET_GRAPHICS` carries the full monitor list. Per monitor rather than one
  wide surface because a monitor with nothing changing then costs nothing, each encode stays at one
  monitor's size, and monitors of different heights don't leave unused area. (H.264 itself allows wide
  frames: level 5.2 caps a frame at 36,864 macroblocks, and 5760x1080 is 24,480. The largest frame
  VideoToolbox accepts hasn't been measured.)
- Frames from each stream go out independently. Flow control (frames in flight, adaptive bitrate) is
  per connection today; phase 1 shares it across surfaces.
- **Legacy bitmap path:** composite all displays into one desktop, since bitmap updates carry desktop
  coordinates. Phase 2.

**Tried and rejected (2026-11-25): one wide surface.** Compositing every display into one
desktop-sized frame on a single surface kept the H.264 code unchanged, but mstsc never split a single
surface across its monitors: it scaled the whole desktop into one monitor (whichever came first in the
Monitor Layout PDU), and with the primary listed first, into both. Listing the primary first in either
message and reporting a 16-monitor maximum (MS-RDPEDISP `MaxNumMonitors`) didn't change that. One surface
per monitor, as xrdp does (`xrdp_mm_egfx_create_surfaces`), works: live on mstsc with a 1920x1080 primary
and a 1366x768 monitor to its right, each monitor shows its own display.

### 4. Input

Each pointer event is hit-tested against the monitor rectangles (client coordinates) and mapped onto that
monitor's display with the existing per-display math. The cursor position the server sends back maps the
other way.

### 5. Live changes

A mid-session layout (`request_layout` with several monitors) re-modes, adds or removes displays and
resets the surfaces, the same way a single-monitor resize does today (deactivation-reactivation).

## Interactions to handle

- **Headless modes** (`--shield-primary`, `--capture-primary`, `--detach-primary`): which display is
  main, where the Dock and menu bar go, and the window gathers (`Ctrl+Alt+G`, the post-resize sweep, and
  `--restore-windows-on-disconnect`) all assume one virtual display.
- **#231/#232:** id following and waking apply per display.
- **Blank recovery** works per connection; a blank on one surface shouldn't force a recovery that
  disturbs the others.
- **Audio** binds to the virtual display; with several, it binds to the primary.
- **The app-switcher HUD** shows on the primary.

## Status (2026-11-25)

Phase 1 works live on mstsc and Windows App on Windows (Windows 11, two monitors; a reconnect from
either picks up the same desktop): the layout is adopted, one virtual display per
monitor, one capture stream and one surface + encoder per monitor (`src/capture_multi.rs`,
`ExtraSurface` in `src/h264.rs`), pointer events go to the monitor they land on. Still to do for phase 1:
live layout changes are ignored and blank recovery is off in these sessions. Each encoder gets its
monitor's area share of the link's (adaptive) bitrate target, so the link carries the target (live: 3
Mbit/s split 1.99 + 1.01 for 1920x1080 + 1366x768), and link-wide keyframe requests (resume, tunnel
recovery and de-migration, the end of an IDR backoff) reach every monitor. mstsc's requested desktop size varies (1920x1080, its primary, or
3284x1080); the session size comes from the layout either way.

## Phases

1. **Two monitors, H.264, `--virtual-display`, no headless mode, mstsc.** The fork change (1), displays
   (2), per-monitor surfaces (3), input (4). Monitor layout fixed for the session.
2. Live layout changes (5), headless modes, Windows App, more than two monitors.
3. Legacy path, a different scale per monitor.

## Testing

- **Unit:** layout normalisation (client coordinates can be negative; EGFX output origins can't, see
  Coordinates), the
  monitor hit-test, display arrangement from a layout.
- **Connection test:** a client announcing two monitors gets a two-monitor Monitor Layout PDU and two
  mapped surfaces.
- **Live:** mstsc with two monitors (different resolutions, primary on either side, one above the other),
  then Windows App. Moving windows across, full-screen video on one, reconnects.

## Coordinates (settled 2026-11-25)

From MS-RDPBCGR 2.2.1.3.6.1 (TS_MONITOR_DEF) and MS-RDPEGFX, and what xrdp (`master` 7a7a112) and
FreeRDP (`master` bd0ccc7) do:

- **Monitor rectangles are primary-relative and signed.** The client's Client Monitor Data puts the
  primary monitor's top-left at (0,0); a monitor to its left or above has negative coordinates;
  right/bottom are inclusive. At most 16 monitors.
- **The Monitor Layout PDU and the `RESET_GRAPHICS` monitor array carry them unchanged.** xrdp
  (`xrdp_caps_send_monitorlayout`, `xrdp_mm_egfx_caps_advertise`) and FreeRDP's server
  (`freerdp_display_send_monitor_layout`) both echo the client's raw rectangles.
- **Everything unsigned is relative to the bounding box's top-left:** the desktop size, surface origins
  (`MapSurfaceToOutput` is unsigned), bitmap updates and input. xrdp keeps a second, shifted copy
  (`minfo_wm`) for these and maps one surface per monitor at the shifted position
  (`xrdp_mm_egfx_create_surfaces`).
- Exactly one monitor is primary, at (0,0); mid-session MS-RDPEDISP monitors are 200–8192 with an even
  width. `RESET_GRAPHICS` needs at least one monitor (xrdp comment, for the Microsoft macOS client).

## Open questions

- What a real Windows server sends in the Monitor Layout PDU (no primary source found; the open-source
  servers echo the client's).
- Whether Windows App for macOS sends Client Monitor Data at all.
- How macOS places the Dock and menu bar across several virtual displays in each headless mode.
