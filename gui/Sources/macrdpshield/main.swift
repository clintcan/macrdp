// macrdpshield — the black shield-window helper for the headless blanking
// modes (--capture-primary / --detach-primary).
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
// Protocol (opcode u8 + big-endian fields), mirroring src/switcher_hud.rs:
//   SHOW(1): [count:u16] then count x [display_id:u32]
//            Shield exactly these displays. Idempotent: re-sending SHOW
//            reconciles (adds new, drops absent) rather than stacking.
//   HIDE(3): no payload. Tear every shield down.
//
// Unknown opcode -> drop the connection so the stream resyncs (same rule as
// macrdphud). Nothing here is persistent state: no gamma, no display capture,
// so if this process dies the shields simply vanish and the panel is normal.

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
    /// display id -> its shield window.
    private var shields: [CGDirectDisplayID: ShieldPanel] = [:]

    /// Reconcile the live shields to exactly `ids`.
    func show(displayIDs ids: [CGDirectDisplayID]) {
        for (id, panel) in shields where !ids.contains(id) {
            panel.orderOut(nil)
            shields.removeValue(forKey: id)
        }
        for id in ids {
            if let existing = shields[id] {
                reframe(existing, on: id)
                existing.orderFrontRegardless()
            } else if let panel = makePanel(for: id) {
                shields[id] = panel
            }
        }
        FileHandle.standardError.write(
            "macrdpshield: SHOW -> \(shields.count) shield(s) up\n".data(using: .utf8)!)
    }

    func hideAll() {
        for (_, panel) in shields { panel.orderOut(nil) }
        shields.removeAll()
        FileHandle.standardError.write("macrdpshield: HIDE -> all shields down\n".data(using: .utf8)!)
    }

    /// A display reconfiguration (the client resizing -> applySettings re-mode)
    /// changes screen frames underneath us. The WINDOW survives it — that is
    /// the whole point of this helper versus gamma — but its frame must be
    /// re-fitted to the panel's new bounds.
    func reframeAll() {
        for (id, panel) in shields {
            reframe(panel, on: id)
            panel.orderFrontRegardless()
        }
    }

    private func makePanel(for id: CGDirectDisplayID) -> ShieldPanel? {
        guard let screen = screen(for: id) else {
            FileHandle.standardError.write(
                "macrdpshield: no NSScreen for display \(id) — not shielding it\n"
                    .data(using: .utf8)!)
            return nil
        }
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

    private func reframe(_ panel: ShieldPanel, on id: CGDirectDisplayID) {
        guard let screen = screen(for: id) else { return }
        panel.setFrame(screen.frame, display: true)
    }

    /// CGDirectDisplayID -> NSScreen, via the screen's NSScreenNumber. Same
    /// lookup macrdphud uses; NSScreen.frame is already global Cocoa
    /// (bottom-left origin) coords, so no CG flip is needed.
    private func screen(for id: CGDirectDisplayID) -> NSScreen? {
        for s in NSScreen.screens {
            let key = NSDeviceDescriptionKey("NSScreenNumber")
            if let n = s.deviceDescription[key] as? NSNumber, n.uint32Value == id {
                return s
            }
        }
        return nil
    }
}

// MARK: - IPC (this process is the SERVER; macrdp connects as a client)

enum ShieldCommand {
    case show([CGDirectDisplayID])
    case hide
}

final class IpcServer {
    private let onCommand: (ShieldCommand) -> Void

    init(onCommand: @escaping (ShieldCommand) -> Void) {
        self.onCommand = onCommand
    }

    func start() {
        Thread.detachNewThread { [weak self] in self?.run() }
    }

    private func run() {
        let fd = socket(AF_INET, SOCK_STREAM, 0)
        guard fd >= 0 else {
            FileHandle.standardError.write("macrdpshield: socket() failed\n".data(using: .utf8)!)
            return
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
        guard bound == 0 else {
            FileHandle.standardError.write(
                "macrdpshield: bind(127.0.0.1:\(SHIELD_PORT)) failed\n".data(using: .utf8)!)
            close(fd)
            return
        }
        guard listen(fd, 4) == 0 else {
            FileHandle.standardError.write("macrdpshield: listen() failed\n".data(using: .utf8)!)
            close(fd)
            return
        }
        FileHandle.standardError.write(
            "macrdpshield: listening on 127.0.0.1:\(SHIELD_PORT)\n".data(using: .utf8)!)

        while true {
            let conn = accept(fd, nil, nil)
            if conn < 0 { continue }
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
            if n <= 0 { return nil }
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

    private func serve(_ conn: Int32) {
        while true {
            guard let op = readFull(conn, 1) else { return }
            switch op[0] {
            case 1:  // SHOW
                guard let head = readFull(conn, 2) else { return }
                let count = Int(be16(head, 0))
                var ids: [CGDirectDisplayID] = []
                ids.reserveCapacity(count)
                for _ in 0..<count {
                    guard let raw = readFull(conn, 4) else { return }
                    ids.append(CGDirectDisplayID(be32(raw, 0)))
                }
                emit(.show(ids))
            case 3:  // HIDE
                emit(.hide)
            default:
                // Unknown opcode: drop the connection so the stream resyncs.
                return
            }
        }
    }

    /// Every command hops to the main thread — AppKit is main-thread-only.
    private func emit(_ cmd: ShieldCommand) {
        DispatchQueue.main.async { self.onCommand(cmd) }
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
                FileHandle.standardError.write(
                    "macrdpshield: parent \(pid) is gone — exiting\n".data(using: .utf8)!)
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

// A display reconfiguration re-frames the shields. The window itself survives
// the re-mode (unlike gamma, which macOS resets), so this is a fit-up, not a
// re-blank.
NotificationCenter.default.addObserver(
    forName: NSApplication.didChangeScreenParametersNotification,
    object: nil,
    queue: .main
) { _ in
    controller.reframeAll()
}

let server = IpcServer { cmd in
    switch cmd {
    case .show(let ids): controller.show(displayIDs: ids)
    case .hide: controller.hideAll()
    }
}
server.start()

if let pid = PARENT_PID { watchParent(pid) }

app.run()
