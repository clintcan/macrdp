// macrdpshield — the black shield-window helper for --shield-primary.
//
// WHY A SEPARATE PROCESS: AppKit windows are main-thread-only and need a
// pumped runloop, and macrdp's main thread is owned by tokio. Same constraint
// that forced macrdphud into its own process; this helper is deliberately
// built to the same shape (helper is the IPC *server*, macrdp is the client,
// parent-pid watchdog, no argv, env-only config).
//
// WHY A WINDOW INSTEAD OF GAMMA: the previous blanking wrote an all-black
// gamma LUT (CGSetDisplayTransferByFormula). macOS **resets the gamma tables
// on every display reconfiguration**, so a live re-mode (the client maximizing
// or moving to a different-resolution monitor) un-blanked the panel, and a
// gamma write issued *during* the reconfiguration does not stick — leaving an
// irreducible ~250 ms desktop flash that no amount of re-asserting could beat.
// A window is not gamma: it survives the reconfiguration, so there is nothing
// to re-assert and no flash.
//
// Protocol (opcode u8 + big-endian fields). Every command gets a REPLY, which
// is the part that matters: a bare write() succeeding proves only that bytes
// reached a socket buffer — not that any display was actually covered. The
// caller needs the achieved count back to decide whether it is safe to proceed.
//   SHOW(1): [exclude_count:u16] then exclude_count x [display_id:u32]
//            Shield EVERY screen except the excluded ones (i.e. except the
//            virtual display the remote client is watching). Idempotent.
//            Reply: [status:u8=0][shielded_count:u16]
//   HIDE(3): no payload. Tear every shield down.
//            Reply: [status:u8=0][shielded_count:u16=0]
//
// SHOW is phrased as an EXCLUDE list, not an include list, on purpose. An
// include list is a snapshot: a monitor plugged in (or a display waking, or one
// whose CGDirectDisplayID changed across sleep) mid-session would never be
// covered, and would quietly show the live remote desktop. Excluding the vd
// instead makes "everything else" the invariant, so the helper re-derives the
// full set from NSScreen.screens on every screen-layout change.
//
// Unknown opcode -> drop the connection so the stream resyncs (same rule as
// macrdphud). Nothing here is persistent state: no gamma, no display capture,
// so if this process dies the shields simply vanish and the panel is normal.
//
// TRUST BOUNDARY: this is an unauthenticated loopback listener, like macrdp's
// other helper channels — but unlike the HUD it controls a PRIVACY mechanism,
// so any local process running as this user can un-blank the panel with a
// single byte. That is consistent with macrdp's documented local security model
// (a hostile same-user process is out of scope: it could read a shared secret
// out of our environment anyway), but it is a real difference in consequence
// and is written up in docs/macos-gotchas.md.

import Cocoa

let SHIELD_PORT: UInt16 = {
    if let s = ProcessInfo.processInfo.environment["MACRDP_SHIELD_PORT"],
       let v = UInt16(s) {
        return v
    }
    return 40244
}()

let PARENT_PID: Int32? = ProcessInfo.processInfo.environment["MACRDP_SHIELD_PARENT"]
    .flatMap { Int32($0) }

func logLine(_ s: String) {
    FileHandle.standardError.write("macrdpshield: \(s)\n".data(using: .utf8)!)
}

// MARK: - Shield windows

/// Borderless black panel pinned above everything except the login window.
///
/// `canBecomeKey`/`canBecomeMain` are false and the style mask carries
/// `.nonactivatingPanel`: the shield must NEVER take key focus, because the
/// RDP session's synthesized keystrokes (CGEventPost) go to whatever app is
/// focused system-wide — a focus-stealing shield would swallow the remote
/// user's typing.
final class ShieldPanel: NSPanel {
    override var canBecomeKey: Bool { false }
    override var canBecomeMain: Bool { false }
}

final class ShieldController {
    private var shields: [CGDirectDisplayID: ShieldPanel] = [:]
    /// Displays never to shield — the virtual display the client is watching.
    private var excluded: Set<CGDirectDisplayID> = []
    /// Whether shielding is currently meant to be active. Held so the
    /// screen-parameters observer knows whether a layout change should
    /// re-derive shields or stay down.
    private var active = false

    /// Activate shielding, excluding `excludeIDs`. Returns the achieved count.
    func show(excluding excludeIDs: Set<CGDirectDisplayID>) -> Int {
        excluded = excludeIDs
        active = true
        return applyShields()
    }

    func hideAll() -> Int {
        active = false
        for (_, panel) in shields { panel.orderOut(nil) }
        shields.removeAll()
        logLine("HIDE -> all shields down")
        return 0
    }

    /// Re-derive the shield set from the CURRENT screen list. Called on SHOW and
    /// on every screen-parameters change, so a monitor attached mid-session (or
    /// a display waking, or one whose id changed) gets covered instead of
    /// quietly displaying the live remote desktop.
    @discardableResult
    func applyShields() -> Int {
        guard active else { return 0 }

        var live: [CGDirectDisplayID: NSScreen] = [:]
        for s in NSScreen.screens {
            let key = NSDeviceDescriptionKey("NSScreenNumber")
            guard let n = s.deviceDescription[key] as? NSNumber else { continue }
            let id = CGDirectDisplayID(n.uint32Value)
            if !excluded.contains(id) { live[id] = s }
        }

        // Drop shields for displays that are gone or newly excluded.
        for (id, panel) in shields where live[id] == nil {
            panel.orderOut(nil)
            shields.removeValue(forKey: id)
        }
        // Add/refit the rest.
        for (id, screen) in live {
            if let existing = shields[id] {
                existing.setFrame(screen.frame, display: true)
                existing.orderFrontRegardless()
            } else {
                let panel = makePanel(on: screen)
                shields[id] = panel
                logLine("shielded display \(id)")
            }
        }
        logLine("SHOW -> \(shields.count) shield(s) up, \(excluded.count) excluded")
        return shields.count
    }

    private func makePanel(on screen: NSScreen) -> ShieldPanel {
        let panel = ShieldPanel(
            contentRect: screen.frame,
            styleMask: [.borderless, .nonactivatingPanel],
            backing: .buffered,
            defer: false,
            screen: screen
        )
        panel.backgroundColor = .black
        panel.isOpaque = true
        panel.hasShadow = false
        panel.isFloatingPanel = true
        panel.hidesOnDeactivate = false
        // Swallow local clicks so someone at the machine cannot poke the
        // desktop that is still being composited underneath the shield.
        // NOTE this only covers clicks landing ON the shield: the pointer is no
        // longer confined (that was the capture's doing), so it can still be
        // walked onto the virtual display, which is deliberately not shielded.
        panel.ignoresMouseEvents = false
        panel.level = NSWindow.Level(rawValue: Int(CGShieldingWindowLevel()))
        panel.collectionBehavior = [
            .canJoinAllSpaces, .stationary, .ignoresCycle, .fullScreenAuxiliary,
        ]
        panel.setFrame(screen.frame, display: true)
        // orderFrontRegardless, never makeKey — see ShieldPanel's note.
        panel.orderFrontRegardless()
        return panel
    }
}

// MARK: - IPC (this process is the SERVER; macrdp connects as a client)

enum ShieldCommand {
    case show(Set<CGDirectDisplayID>)
    case hide
}

final class IpcServer {
    /// Returns the achieved shield count, which is sent back as the reply.
    private let onCommand: (ShieldCommand) -> Int

    init(onCommand: @escaping (ShieldCommand) -> Int) {
        self.onCommand = onCommand
    }

    func start() {
        Thread.detachNewThread { [weak self] in self?.run() }
    }

    private func run() {
        let fd = socket(AF_INET, SOCK_STREAM, 0)
        guard fd >= 0 else {
            logLine("socket() failed — exiting")
            exit(1)
        }
        var yes: Int32 = 1
        setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &yes, socklen_t(MemoryLayout<Int32>.size))

        var addr = sockaddr_in()
        addr.sin_family = sa_family_t(AF_INET)
        addr.sin_port = SHIELD_PORT.bigEndian
        addr.sin_addr.s_addr = inet_addr("127.0.0.1")

        let bound = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                bind(fd, $0, socklen_t(MemoryLayout<sockaddr_in>.size))
            }
        }
        // EXIT, don't limp on. A helper that stays alive with no listener is
        // worse than no helper at all: macrdp's connect would land on whatever
        // process DID get the port (a stale helper from a previous run, or a
        // squatter), its write would succeed, and macrdp would report a
        // successfully shielded desktop that is in fact fully visible. Exiting
        // makes the failure loud — macrdp's connect then fails outright.
        guard bound == 0 else {
            logLine("bind(127.0.0.1:\(SHIELD_PORT)) failed — port already in use? exiting")
            close(fd)
            exit(1)
        }
        guard listen(fd, 4) == 0 else {
            logLine("listen() failed — exiting")
            close(fd)
            exit(1)
        }
        logLine("listening on 127.0.0.1:\(SHIELD_PORT)")

        while true {
            let conn = accept(fd, nil, nil)
            if conn < 0 { continue }
            // Bound every blocking recv on this connection. Without it, ONE
            // peer that connects and never writes (a port scanner, a stray nc,
            // or macrdp SIGKILLed between connect and write) wedges this
            // single-threaded accept loop forever — and because the kernel
            // still completes handshakes into the backlog, macrdp's later
            // SHOW/HIDE would appear to succeed while nothing ever applied.
            var tv = timeval(tv_sec: 5, tv_usec: 0)
            setsockopt(conn, SOL_SOCKET, SO_RCVTIMEO, &tv, socklen_t(MemoryLayout<timeval>.size))
            serve(conn)
            close(conn)
        }
    }

    private func readFull(_ fd: Int32, _ count: Int) -> [UInt8]? {
        var buf = [UInt8](repeating: 0, count: count)
        var got = 0
        while got < count {
            let n = buf.withUnsafeMutableBytes { raw -> Int in
                recv(fd, raw.baseAddress!.advanced(by: got), count - got, 0)
            }
            if n <= 0 { return nil }  // EOF, error, or the recv timeout above
            got += n
        }
        return buf
    }

    private func be16(_ b: [UInt8], _ i: Int) -> UInt16 {
        (UInt16(b[i]) << 8) | UInt16(b[i + 1])
    }

    private func be32(_ b: [UInt8], _ i: Int) -> UInt32 {
        (UInt32(b[i]) << 24) | (UInt32(b[i + 1]) << 16) | (UInt32(b[i + 2]) << 8) | UInt32(b[i + 3])
    }

    private func reply(_ fd: Int32, count: Int) {
        let c = UInt16(clamping: count)
        let out: [UInt8] = [0, UInt8(c >> 8), UInt8(c & 0xff)]
        _ = out.withUnsafeBytes { raw in send(fd, raw.baseAddress!, raw.count, 0) }
    }

    private func serve(_ conn: Int32) {
        while true {
            guard let op = readFull(conn, 1) else { return }
            switch op[0] {
            case 1:  // SHOW
                guard let head = readFull(conn, 2) else { return }
                let count = Int(be16(head, 0))
                var ids = Set<CGDirectDisplayID>()
                for _ in 0..<count {
                    guard let raw = readFull(conn, 4) else { return }
                    ids.insert(CGDirectDisplayID(be32(raw, 0)))
                }
                reply(conn, count: emit(.show(ids)))
            case 3:  // HIDE
                reply(conn, count: emit(.hide))
            default:
                // Unknown opcode: drop the connection so the stream resyncs.
                return
            }
        }
    }

    /// Run the command on the main thread (AppKit is main-thread-only) and wait
    /// for its result, so the reply carries the ACHIEVED count rather than an
    /// optimistic acknowledgement. `sync` is safe here: this runs on the IPC
    /// thread, never on main.
    private func emit(_ cmd: ShieldCommand) -> Int {
        var result = 0
        DispatchQueue.main.sync { result = self.onCommand(cmd) }
        return result
    }
}

// MARK: - Parent watchdog

/// Exit if macrdp dies, so a crashed parent cannot leave a black screen
/// stranded over the user's desktop with no way to dismiss it. This is the
/// single most safety-critical line in the helper.
func watchParent(_ pid: Int32) {
    Thread.detachNewThread {
        while true {
            if kill(pid, 0) != 0 {
                logLine("parent \(pid) is gone — exiting")
                exit(0)
            }
            Thread.sleep(forTimeInterval: 1.0)
        }
    }
}

// MARK: - Entry

let app = NSApplication.shared
app.setActivationPolicy(.accessory)

let controller = ShieldController()

// Re-derive shields whenever the screen layout changes: a display attached,
// removed, woken, or re-moded. This is what covers a monitor plugged in
// mid-session. macrdp ALSO pushes a SHOW from its capture path after a
// virtual-display re-mode, because the private CGVirtualDisplay applySettings
// is known not to emit a public reconfiguration event (see the removed-callback
// NOTE in src/virtual_display/mod.rs) and this notification may likewise not
// fire for it.
NotificationCenter.default.addObserver(
    forName: NSApplication.didChangeScreenParametersNotification,
    object: nil,
    queue: .main
) { _ in
    controller.applyShields()
}

let server = IpcServer { cmd in
    switch cmd {
    case .show(let excluded): return controller.show(excluding: excluded)
    case .hide: return controller.hideAll()
    }
}
server.start()

if let pid = PARENT_PID { watchParent(pid) }

app.run()
