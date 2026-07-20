// shieldprobe.swift — de-risking probe for Option B (black shield window
// replacing CGDisplayCapture + gamma blanking for --capture-primary).
//
// The ONLY question this answers: with a black window at shielding level
// covering a physical display and NO CGDisplayCapture in effect, can the Mac
// still lock? (CGDisplayCapture demonstrably prevents locking — measured
// 2026-07-20, ~340 samples. A shield window is expected NOT to, since the
// screensaver uses the same mechanism and locking works with it.)
//
// Run:  swift shieldprobe.swift [seconds]      (default 40)
// It shields every display EXCEPT one you can still use, prints the lock
// state once a second, and tears itself down on a timer no matter what.
//
// SAFETY: this does NOT lock anything by itself. It only observes. You do the
// locking manually. Nothing here is persistent: the windows vanish when the
// process exits, and there is no gamma change and no display capture, so a
// crash or a kill -9 leaves nothing to clean up.

import Cocoa

let seconds = CommandLine.arguments.count > 1 ? (Double(CommandLine.arguments[1]) ?? 40) : 40

// Is the session locked right now? Same signal the original investigation
// used: the CGSSessionScreenIsLocked key in the session dictionary.
func screenIsLocked() -> Bool {
    guard let d = CGSessionCopyCurrentDictionary() as? [String: Any] else { return false }
    return (d["CGSSessionScreenIsLocked"] as? Int) == 1
}

let app = NSApplication.shared
app.setActivationPolicy(.accessory) // no Dock icon, never activates

var windows: [NSWindow] = []
for screen in NSScreen.screens {
    let w = NSWindow(
        contentRect: screen.frame,
        styleMask: [.borderless],
        backing: .buffered,
        defer: false,
        screen: screen
    )
    w.backgroundColor = .black
    w.isOpaque = true
    w.hasShadow = false
    w.ignoresMouseEvents = false          // swallow local clicks
    w.level = NSWindow.Level(rawValue: Int(CGShieldingWindowLevel()))
    w.collectionBehavior = [.canJoinAllSpaces, .stationary, .ignoresCycle]
    w.setFrame(screen.frame, display: true)
    w.orderFrontRegardless()               // show WITHOUT taking key focus
    windows.append(w)
}

FileHandle.standardError.write("""
shieldprobe: \(windows.count) shield window(s) up at level \(CGShieldingWindowLevel()).
shieldprobe: NO CGDisplayCapture, NO gamma change — nothing to clean up if this dies.
shieldprobe: now try to lock (Apple menu -> Lock Screen, or Ctrl-Cmd-Q) and watch below.
shieldprobe: auto-teardown in \(Int(seconds))s.

""".data(using: .utf8)!)

var ticks = 0
var sawLocked = false
let timer = Timer.scheduledTimer(withTimeInterval: 1.0, repeats: true) { t in
    ticks += 1
    let locked = screenIsLocked()
    if locked { sawLocked = true }
    FileHandle.standardError.write(
        "t=\(ticks)s locked=\(locked)\n".data(using: .utf8)!)
    if Double(ticks) >= seconds {
        t.invalidate()
        for w in windows { w.orderOut(nil) }
        let verdict = sawLocked
            ? "RESULT: LOCKED at least once while shielded -> shield does NOT block locking. Option B is viable."
            : "RESULT: never observed locked. Either you did not try to lock, or the shield blocks it (same as capture)."
        FileHandle.standardError.write("\n\(verdict)\n".data(using: .utf8)!)
        app.terminate(nil)
    }
}
RunLoop.current.add(timer, forMode: .common)
app.run()
