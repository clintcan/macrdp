# Video: H.264/EGFX, display resolution, and aspect handling

How macrdp draws your screen: the H.264 pipeline (`--enable-h264`), Retina
capture (`--hidpi`), client-resolution auto-adopt and letterboxing, the mstsc
reconnect quirk and its fixes, and the color-conversion implementation notes.

## Display resolution (`--hidpi`)

By default macrdp captures and advertises the Mac's **logical** resolution — the points it reports in System Settings (e.g. 1512×982 on a default-scaled 14" MacBook). On a Retina panel that's half the physical pixels, so any client whose window is larger upscales it and text looks soft.

Pass **`--hidpi`** to capture at the display's **backing (Retina) pixel resolution** instead (e.g. 3024×1964) — clients then render crisp native pixels. It's **opt-in** because it's ~4× the pixels:

- **Pair it with `--enable-h264`.** H.264 compresses the higher resolution cleanly and the client downscales it sharply — that's the real "Retina remote desktop" experience. On the legacy bitmap path it just means 4× the bandwidth.
- **mstsc feels laggy at HiDPI.** mstsc decodes 4× the pixels every frame and its ~2-frame presentation buffer now holds 4×-bigger frames, so responsiveness drops. **Thincast / FreeRDP stay snappy** — their H.264 decoders keep up. The server itself isn't the bottleneck (it encodes a 3024×1964 frame in ~10 ms, well inside the 60fps budget); the cost is client-side decode. Prefer a capable client if you want HiDPI.
- Ignored when you pass explicit `--width`/`--height` (you've chosen the size) or with `--virtual-display` (already an explicit resolution).

Input and cursor are resolution-correct at any setting — clicks land precisely and the pointer stays normal-sized.

### Aspect ratio (auto-size path)

By default macrdp serves exactly the resolution the connecting client requests (e.g. mstsc full-screen on a 1920×1080 monitor gets a 1920×1080 session). When that resolution's aspect ratio differs from the Mac's panel (e.g. a 16:9 client against a 16:10 MacBook), macrdp **preserves the Mac's aspect ratio and adds black bars** (letterbox top/bottom or pillarbox left/right) so the picture isn't distorted, and maps mouse input into the centered picture so clicks stay accurate. Verified: a 1512×982 Mac served to a 1920×1080 client produces a centered 1663×1080 image with 128 px bars each side.

Pass **`--stretch`** to instead fill the whole frame (the old behavior) — no bars, but the image is non-uniformly scaled on an aspect mismatch (e.g. ~13.5% vertical compression for 16:10→16:9). `--stretch` has no effect when the aspect already matches, or with explicit `--width`/`--height` (those always stretch). Either way, serving a non-native resolution forces full-frame updates (higher bandwidth) and, on **mstsc with `--enable-h264`**, the scaling amplifies its trailing-frame presentation lag — a Mac whose native resolution already matches the client (no scaling) is snappier. See "Video".

## Video (H.264)

By default the display is sent as legacy bitmaps (RemoteFx/QOI to mstsc, NSCodec/raw to others) — works everywhere, but bandwidth-heavy. Pass **`--enable-h264`** to stream the desktop as **H.264 over the EGFX virtual channel** (MS-RDPEGFX, AVC420), hardware-encoded with VideoToolbox. Far less bandwidth, especially for video/scrolling/photos.

How it behaves:

- **Automatic fallback.** Clients that don't advertise H.264 (AVC420) decode — e.g. a FreeRDP build without an H.264 decoder — transparently fall back to legacy bitmaps. No need to match the flag to the client. mstsc, FreeRDP-with-H.264, and the macOS **Windows App** / Microsoft Remote Desktop client all decode the H.264 stream.
- **Wire format.** The AVC420 payload is Annex-B framed (what Microsoft's decoder expects). The bitstream is verified rendering on `mstsc` and on FreeRDP built with H.264 (e.g. the [Thincast client]).
- **Bitrate.** `--bitrate N` sets the target encoder bitrate in megabits/sec (default `6`, only meaningful with `--enable-h264`). Raising it sharpens detail but grows each frame, so the big per-frame writes are more likely to fill the socket buffer and delay audio on a constrained link — `6` is a good balance; try `8`–`12` if you have headroom.
- **Color.** The stream is encoded as full-range BT.709. This matters for `mstsc`, which reads AVC420 luma as full-range regardless of the bitstream flag — video-range output otherwise renders washed-out / lighter there. FreeRDP honors the flag and is correct either way. To get full range we convert each captured BGRA frame to full-range NV12 ourselves (VideoToolbox would otherwise emit video-range from a BGRA source); that conversion is **vImage**-accelerated — see [Color conversion: scalar vs vImage](#color-conversion-scalar-vs-vimage).
- **Frame rate.** `--enable-h264` defaults to **60fps** (vs 15 for legacy). mstsc holds a fixed ~2-frame presentation buffer for the H.264 stream, so at 30fps typing lags ~2 keystrokes (~66ms) while at 60fps that buffer is ~33ms and feels immediate. FreeRDP-based clients don't buffer this way and are snappy at any rate. Set `--fps` explicitly to override (lower it to save CPU/bandwidth if your client/link doesn't need 60).
- **Keyframes.** A keyframe (IDR) is forced on the first frame, then periodically every `--keyframe-interval` seconds (default `2`) as a safety net — some clients (mstsc) only fully recover a transient decode glitch on the next IDR, so a long interval leaves garbled regions (notably text) lingering. Lower it for faster recovery at the cost of bandwidth/quality; raise it for smoother typing. Optionally, pass **`--keyframe-on-change`** (off by default) to additionally force an IDR whenever a large area changes at once (window-to-front, scroll, app launch) and briefly after a mouse click, so big updates land immediately instead of waiting for the periodic interval (rising-edge detection keeps sustained churn like video from forcing an IDR every frame). It's off by default because the periodic interval plus the trailing flush-burst (`--flush-frames`) already drain mstsc's presentation buffer, so the extra forced IDRs mostly just spend bitrate/quality at a fixed bitrate for no typing benefit — enable it only if large updates visibly lag on your client/link. When enabled, the trigger thresholds are tunable: `--keyframe-change-pct` (default 20, the dirty-area % that fires an IDR), `--keyframe-click-pct` (default 5, the lowered threshold after a click), and `--keyframe-click-window-ms` (default 400, how long that lowered threshold lasts).
- **Flush frames (`--flush-frames`, default `4`).** ScreenCaptureKit only delivers a frame when the screen changes, so after the last keystroke before a pause there are no further frames to push it through mstsc's ~2-frame AVC420 presentation buffer — it would strand there until the next change or periodic keyframe (the classic "typing follows the keyframe" lag). After each change the server re-submits the last frame this many times as cheap skip-P-frames, draining the buffer so the change appears within a couple of frame intervals (~33 ms at 60fps), then goes quiet. mstsc needs ≥2; raise if a slight trailing lag remains, or set `0` to disable.

### Known limitations

- **Reconnecting `mstsc` to a still-running macrdp can show a black screen** (with a live cursor). This is an mstsc-specific quirk: it retains EGFX surfaces for the lifetime of its process and mis-composites on reconnect. It is *not* a server bug — FreeRDP reconnects cleanly over the same stream. **The server now self-heals it in place** (default, with `--enable-h264`): on a detected blank it sends a bare core RDP Deactivation–Reactivation that preserves the EGFX channel/surface, and mstsc re-maps its retained surface and presents again in ~1–2 s with no disconnect (see the blank-recovery notes in [known-quirks.md](known-quirks.md)). If the automatic recovery ever fails to heal, **close + reopen the mstsc window** — quitting the client clears its surface cache, so the desktop renders every time (no Windows reboot needed). (A server-side fresh-surface-id workaround, and an earlier fork-a-fresh-worker-process-per-connection model, were both tried before the in-place reactivation superseded them.)
- H.264 is **macOS-only** (VideoToolbox) and still maturing — bitrate and keyframe behavior are tunable (above), but dirty-region *encoding* is not yet done: every frame is a full encode (dirty rects are used only to time on-demand keyframes, not to encode sub-regions). H.264's own inter-prediction keeps unchanged regions cheap regardless.

### Color conversion: scalar vs vImage

*(Implementation detail — skip unless you're profiling CPU or porting the encoder.)*

VideoToolbox, given a BGRA source, emits **video-range** YUV (luma 16–235). `mstsc` reads AVC420 luma as **full-range**, so that looks washed out (see **Color** above). The fix is to hand VideoToolbox a YUV buffer that's already full-range, which means doing the BGRA → full-range BT.709 NV12 (`420f`) color conversion ourselves, once per captured frame, on the capture thread.

That conversion is a real per-frame cost, so it's done with **vImage** (Apple's Accelerate framework), which runs the RGB→Y'CbCr math on the CPU's vector units (NEON on Apple Silicon). A scalar reference implementation (a plain Rust loop) is kept as well: it's the fallback for any frame vImage declines (e.g. odd dimensions), the oracle the vImage path is unit-tested against, and the baseline below. Both produce identical output (within ±1 rounding).

Single-thread cost per frame, Apple M3 (`cargo test --release bench_nv12_full_range -- --ignored --nocapture`):

| Resolution | scalar | vImage | speedup |
|---|---:|---:|---:|
| 1470×956 | 3.36 ms | 0.12 ms | ~29× |
| 1920×1080 | 4.98 ms | 0.16 ms | ~32× |
| 2560×1440 | 8.88 ms | 0.33 ms | ~27× |
| 3840×2160 (4K) | 20.0 ms | 0.84 ms | ~24× |

At 60fps the frame budget is 16.67 ms. The scalar path is fine at 1080p (~30% of one core) but **exceeds the budget at 4K**, where it would cap the achievable frame rate before the encoder even runs; vImage keeps the conversion at ~1% of budget across the board, so it's never the bottleneck. The implementation lives in `src/videotoolbox.rs` (`bgra_to_nv12_full_range_vimage`, with `bgra_to_nv12_full_range` as the scalar reference).

[Thincast client]: https://thincast.com/en/products/client
