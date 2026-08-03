import AppKit
import UniformTypeIdentifiers

// macrdp Controller: a menu-bar app that controls the macrdp LaunchAgent
// (label com.clintcan.macrdp, installed by packaging/install-launchagent.sh)
// and toggles flags in config.env. It is a *controller* — quitting it leaves
// the server running under launchd. It needs no TCC grants of its own (it only
// runs `launchctl`, opens URLs, and edits files in the user's own Library);
// the Screen Recording / Accessibility grants belong to the macrdp binary.

final class AppController: NSObject, NSApplicationDelegate, NSMenuDelegate {
    // Lazy so the controller can be instantiated for the headless --install-agent
    // path without touching the status bar (which needs a GUI app context).
    lazy var statusItem = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)

    /// The server's LaunchAgent label, derived from this controller's own bundle
    /// id by stripping the ".controller" suffix — so whatever BUNDLE_PREFIX the
    /// app was built with, the controller drives the matching agent. Falls back
    /// to the default prefix for unbundled `swift run` during development.
    let label: String = {
        if let bid = Bundle.main.bundleIdentifier, bid.hasSuffix(".controller") {
            return String(bid.dropLast(".controller".count))
        }
        return "com.clintcan.macrdp"
    }()

    var uid: String { String(getuid()) }
    var domain: String { "gui/\(uid)" }
    var service: String { "gui/\(uid)/\(label)" }

    var home: URL { FileManager.default.homeDirectoryForCurrentUser }
    var configURL: URL { home.appendingPathComponent("Library/Application Support/macrdp/config.env") }
    var logURL: URL { home.appendingPathComponent("Library/Logs/macrdp.log") }
    // The server owns + rotates macrdp.log itself; stderr (panics, pre-logging
    // startup errors) goes to a small separate file.
    var errLogURL: URL { home.appendingPathComponent("Library/Logs/macrdp.err.log") }
    var plistURL: URL { home.appendingPathComponent("Library/LaunchAgents/\(label).plist") }

    var timer: Timer?

    /// The tabbed Settings window (SettingsWindow.swift); nil while closed.
    var settingsWindowController: SettingsWindowController?

    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.accessory) // menu-bar only, no Dock icon
        installMainMenu() // so the Settings window's text fields get edit shortcuts
        if let button = statusItem.button {
            button.image = NSImage(systemSymbolName: "display", accessibilityDescription: "macrdp")
            button.image?.isTemplate = true
        }
        let menu = NSMenu()
        menu.delegate = self          // menuNeedsUpdate rebuilds on every open
        statusItem.menu = menu
        rebuildMenu()
        refreshGlyph()
        timer = Timer.scheduledTimer(withTimeInterval: 5.0, repeats: true) { [weak self] _ in
            self?.refreshGlyph()
        }
    }

    // MARK: - Agent state

    /// (loaded, pid). loaded=false means the agent isn't bootstrapped at all;
    /// pid=nil while loaded means installed but not currently running.
    func agentState() -> (loaded: Bool, pid: Int?) {
        let out = run("/bin/launchctl", ["print", service])
        guard out.code == 0 else { return (false, nil) }
        if let r = out.stdout.range(of: #"pid = (\d+)"#, options: .regularExpression) {
            let pid = out.stdout[r].split(separator: "=").last
                .flatMap { Int($0.trimmingCharacters(in: .whitespaces)) }
            return (true, pid)
        }
        return (true, nil)
    }

    func refreshGlyph() {
        let st = agentState()
        let running = st.pid != nil
        // Dim the menu-bar icon when the server isn't running so state is
        // glanceable without opening the menu.
        statusItem.button?.alphaValue = running ? 1.0 : 0.4
        statusItem.button?.toolTip = running
            ? "macrdp: running (pid \(st.pid!))"
            : (st.loaded ? "macrdp: stopped" : "macrdp: not installed")
    }

    // MARK: - Server status (parsed from the log)

    /// Last ~32 KB of the server log split into lines (oldest first).
    func logTail() -> [String] {
        guard let h = try? FileHandle(forReadingFrom: logURL) else { return [] }
        defer { try? h.close() }
        let size = (try? h.seekToEnd()) ?? 0
        let window: UInt64 = 32 * 1024
        try? h.seek(toOffset: size > window ? size - window : 0)
        let data = (try? h.readToEnd()) ?? Data()
        return (String(data: data, encoding: .utf8) ?? "")
            .split(separator: "\n", omittingEmptySubsequences: true).map(String.init)
    }

    /// Latest TCC grant state the server logged (nil = not seen in recent log).
    /// The server logs "<X> permission already granted" / "<X> permission NOT
    /// granted" at startup; we can't query another process's TCC directly.
    func permissionStatus() -> (screen: Bool?, accessibility: Bool?) {
        var screen: Bool?
        var ax: Bool?
        for line in logTail() { // later lines win → most recent startup
            if line.contains("Screen Recording permission already granted") { screen = true } else if line
                .contains("Screen Recording permission NOT granted") { screen = false }
            if line.contains("Accessibility permission already granted") { ax = true } else if line
                .contains("Accessibility permission NOT granted") { ax = false }
        }
        return (screen, ax)
    }

    /// Most recent error worth surfacing (auth failure / port in use / panic).
    func lastServerError() -> String? {
        for raw in logTail().reversed() {
            let line = raw.replacingOccurrences(
                of: "\u{1b}\\[[0-9;]*m", with: "", options: .regularExpression)
            if line.contains("authentication failed") { return "Login failed — check the account password" }
            if line.contains("Address already in use") { return "Port in use — another server is bound to :3390" }
            if line.contains("panicked") { return "Server crashed — see Open Logs" }
        }
        return nil
    }

    // MARK: - Menu

    func menuNeedsUpdate(_ menu: NSMenu) { rebuildMenu() }

    func rebuildMenu() {
        guard let menu = statusItem.menu else { return }
        menu.removeAllItems()

        let st = agentState()
        let header: String
        if !st.loaded { header = "macrdp — not installed" }
        else if let pid = st.pid { header = "macrdp — running (pid \(pid))" }
        else { header = "macrdp — stopped" }
        let h = NSMenuItem(title: header, action: nil, keyEquivalent: "")
        h.isEnabled = false
        menu.addItem(h)
        // Surface a server error (auth/port/crash) right under the header so a
        // silent crash-loop isn't invisible.
        if st.pid == nil, let err = lastServerError() {
            let e = NSMenuItem(title: "⚠️ \(err)", action: nil, keyEquivalent: "")
            e.isEnabled = false
            menu.addItem(e)
        }
        menu.addItem(.separator())

        // Show the tabbed Settings window (where all the config options now live).
        menu.addItem(item("Show macrdp…", #selector(showSettings)))
        menu.addItem(.separator())

        let running = st.pid != nil
        if running {
            menu.addItem(item("Stop", #selector(stop)))
            menu.addItem(item("Restart", #selector(restart)))
        } else {
            // Start self-installs the LaunchAgent + onboards the password on
            // first run, so it's always actionable (no Terminal step needed).
            menu.addItem(item(st.loaded ? "Start" : "Start (first run sets up)", #selector(start)))
        }
        menu.addItem(.separator())
        menu.addItem(item("Quit Controller", #selector(quit)))
    }

    func item(_ title: String, _ sel: Selector) -> NSMenuItem {
        let i = NSMenuItem(title: title, action: sel, keyEquivalent: "")
        i.target = self
        return i
    }

    // MARK: - Actions

    @objc func start() {
        // Self-install on first run: locate the server app, onboard the Keychain
        // password, write + register the LaunchAgent — no Terminal step needed.
        guard let serverApp = locateServerApp() else {
            alert(style: .warning, "Can't find macrdp.app",
                  "Move both macrdp.app and macrdp Controller into /Applications "
                  + "(or ~/Applications), then click Start again.")
            return
        }
        if !hasKeychainPassword() {
            guard promptAndStorePassword() else { return } // user cancelled
        }
        ensureConfigExists()
        let firstInstall = !FileManager.default.fileExists(atPath: plistURL.path)
        if firstInstall { installLaunchAgent(serverApp: serverApp) }
        ensureLoaded()
        _ = run("/bin/launchctl", ["kickstart", "-k", service])
        refreshGlyph()
        if firstInstall { remindPermissions() }
    }

    @objc func stop() {
        _ = run("/bin/launchctl", ["bootout", service])
        refreshGlyph()
    }

    @objc func restart() { start() }

    func ensureLoaded() {
        guard !agentState().loaded, FileManager.default.fileExists(atPath: plistURL.path) else { return }
        // `launchctl bootstrap` intermittently fails with EIO ("Bootstrap
        // failed: 5: Input/output error") right after a bootout; retry a few
        // times until the service registers.
        for _ in 0..<5 {
            _ = run("/bin/launchctl", ["bootstrap", domain, plistURL.path])
            if agentState().loaded { break }
            Thread.sleep(forTimeInterval: 0.5)
        }
        _ = run("/bin/launchctl", ["enable", service])
    }

    // MARK: - Headless entry (scripted/MDM deploy + testing)

    /// Runs the install logic without the GUI. `--print-paths` is side-effect
    /// free; `--install-agent` locates the server, writes + loads the agent
    /// (assumes the Keychain password is set separately for unattended deploys).
    func runHeadless(_ args: [String]) -> Int32 {
        if args.contains("--print-paths") {
            print("label:      \(label)")
            print("bind:       \(readConfig()["BIND"] ?? "127.0.0.1:3390")")
            print("server app: \(locateServerApp()?.path ?? "NOT FOUND")")
            print("plist:      \(plistURL.path)")
            print("config:     \(configURL.path)")
            print("log:        \(logURL.path)")
            print("password:   \(hasKeychainPassword() ? "set" : "MISSING")")
            return 0
        }
        guard let serverApp = locateServerApp() else {
            FileHandle.standardError.write(Data(
                "error: macrdp.app not found next to the controller or in /Applications\n".utf8))
            return 1
        }
        ensureConfigExists()
        installLaunchAgent(serverApp: serverApp)
        ensureLoaded()
        _ = run("/bin/launchctl", ["kickstart", "-k", service])
        print("installed: \(plistURL.path) -> \(serverApp.path)")
        if !hasKeychainPassword() {
            print("note: Keychain password not set — store it with:")
            print("  security add-generic-password -U -s macrdp -a \(NSUserName()) -w '<password>'")
        }
        return 0
    }

    // MARK: - Self-install

    /// Locate the server bundle (`macrdp.app`): next to this controller first
    /// (the usual case — both dragged into the same folder), then the standard
    /// install locations.
    func locateServerApp() -> URL? {
        let candidates = [
            Bundle.main.bundleURL.deletingLastPathComponent().appendingPathComponent("macrdp.app"),
            URL(fileURLWithPath: "/Applications/macrdp.app"),
            home.appendingPathComponent("Applications/macrdp.app"),
        ]
        let fm = FileManager.default
        return candidates.first {
            fm.fileExists(atPath: $0.appendingPathComponent("Contents/MacOS/macrdp").path)
        }
    }

    /// Write + register the LaunchAgent plist pointing at the located server's
    /// SIGNED binary, run directly with `--config` (the binary reads config.env
    /// itself). Mirrors packaging/install-launchagent.sh, in-process. Launching
    /// the signed Mach-O — not an unsigned wrapper script — gives macOS
    /// Background Task Management a stable identity to approve once.
    func installLaunchAgent(serverApp: URL) {
        let bin = serverApp.appendingPathComponent("Contents/MacOS/macrdp").path
        let dict: [String: Any] = [
            "Label": label,
            "ProgramArguments": [bin, "--config", configURL.path],
            "RunAtLoad": true,
            "KeepAlive": true,
            // No StandardOutPath: the server writes + rotates macrdp.log itself
            // (a second writer would corrupt it / strand launchd on a rotated
            // inode). StandardErrorPath keeps panics + pre-logging stderr.
            "StandardErrorPath": errLogURL.path,
            "EnvironmentVariables": ["RUST_LOG": "info"],
        ]
        let fm = FileManager.default
        try? fm.createDirectory(at: plistURL.deletingLastPathComponent(),
                                withIntermediateDirectories: true)
        try? fm.createDirectory(at: logURL.deletingLastPathComponent(),
                                withIntermediateDirectories: true)
        if let data = try? PropertyListSerialization.data(fromPropertyList: dict,
                                                          format: .xml, options: 0) {
            try? data.write(to: plistURL)
        }
    }

    // MARK: - Keychain password onboarding

    /// The server (run headless by launchd) reads its account password from the
    /// Keychain via the `security` CLI, so we write it the same way — keeping the
    /// item's access context as /usr/bin/security so no read-time prompt appears.
    func hasKeychainPassword() -> Bool {
        run("/usr/bin/security", ["find-generic-password", "-s", "macrdp", "-a", NSUserName()]).code == 0
    }

    @discardableResult
    func promptAndStorePassword() -> Bool {
        let a = NSAlert()
        a.messageText = "Enter your macOS account password"
        a.informativeText = "macrdp authenticates RDP clients against your Mac account and "
            + "starts headless via launchd, so the password is stored in your login Keychain. "
            + "It never leaves this Mac."
        a.addButton(withTitle: "Save")
        a.addButton(withTitle: "Cancel")
        let field = NSSecureTextField(frame: NSRect(x: 0, y: 0, width: 260, height: 24))
        field.placeholderString = "Account password for \(NSUserName())"
        a.accessoryView = field
        a.window.initialFirstResponder = field
        NSApp.activate(ignoringOtherApps: true)
        guard a.runModal() == .alertFirstButtonReturn, !field.stringValue.isEmpty else { return false }
        let r = run("/usr/bin/security",
                    ["add-generic-password", "-U", "-s", "macrdp", "-a", NSUserName(),
                     "-w", field.stringValue])
        if r.code != 0 {
            alert(style: .critical, "Couldn't save password", "Keychain returned an error.")
            return false
        }
        return true
    }

    @objc func setPassword() { promptAndStorePassword() }

    func remindPermissions() {
        let a = NSAlert()
        a.messageText = "Grant macrdp two permissions"
        a.informativeText = "macrdp needs Screen Recording (to share the display) and "
            + "Accessibility (to forward keyboard/mouse). Enable macrdp.app in System "
            + "Settings → Privacy & Security, then it'll work."
        a.addButton(withTitle: "Open Privacy Settings")
        a.addButton(withTitle: "Later")
        NSApp.activate(ignoringOtherApps: true)
        if a.runModal() == .alertFirstButtonReturn { openScreenRecording() }
    }

    func alert(style: NSAlert.Style, _ message: String, _ info: String) {
        let a = NSAlert()
        a.alertStyle = style
        a.messageText = message
        a.informativeText = info
        a.addButton(withTitle: "OK")
        NSApp.activate(ignoringOtherApps: true)
        _ = a.runModal()
    }

    /// An attached USB device, for the smart-card trigger picker.
    struct UsbDevice {
        let vid: String // "0x2174" (4-digit lowercase hex)
        let pid: String // "0x2100"
        let label: String // human-readable name
    }

    /// Enumerate attached USB devices via `ioreg -a -r -c IOUSBHostDevice`, which
    /// emits an XML-plist array of device dicts (idVendor/idProduct as decimal
    /// ints, plus name strings). We deliberately use ioreg, NOT
    /// `system_profiler SPUSBDataType`: some USB-C devices (e.g. a Transcend
    /// ESD310C SSD — the dev trigger) are visible in ioreg but never appear in
    /// the SPUSBDataType tree, so a system_profiler-based picker silently misses
    /// them. install-ifd-handler.sh's hint dump already uses ioreg for the same
    /// reason. VID/PID are formatted as the 4-digit lowercase hex the bundle's
    /// Info.plist wants.
    func usbDevices() -> [UsbDevice] {
        let out = run("/usr/sbin/ioreg", ["-a", "-r", "-c", "IOUSBHostDevice"])
        guard out.code == 0, let data = out.stdout.data(using: .utf8),
              let list = try? PropertyListSerialization.propertyList(
                  from: data, options: [], format: nil) as? [[String: Any]] else { return [] }
        func hex4(_ any: Any?) -> String? {
            // idVendor/idProduct come as NSNumber; tolerate a string too.
            if let n = any as? Int { return String(format: "0x%04x", n & 0xFFFF) }
            if let s = any as? String, let n = Int(s) { return String(format: "0x%04x", n & 0xFFFF) }
            return nil
        }
        var devices: [UsbDevice] = []
        var seen = Set<String>()
        for it in list {
            guard let vid = hex4(it["idVendor"]), let pid = hex4(it["idProduct"]) else { continue }
            let name = (it["USB Product Name"] as? String)
                ?? (it["IORegistryEntryName"] as? String) ?? "USB device"
            let man = (it["USB Vendor Name"] as? String) ?? ""
            let label = (man.isEmpty || name.contains(man)) ? name : "\(name) (\(man))"
            // De-dupe identical VID/PID (e.g. two of the same stick) by key.
            let key = "\(vid):\(pid):\(label)"
            if seen.insert(key).inserted { devices.append(UsbDevice(vid: vid, pid: pid, label: label)) }
        }
        return devices
    }

    enum TriggerChoice {
        case cancel // abort the install
        case keepDefault // install, leave the bundle's baked-in trigger
        case device(vid: String, pid: String) // install, rebind to this device
    }

    /// Show a native popup of attached USB devices to use as the smart-card load
    /// trigger (macOS loads the IFD driver only on a USB hotplug whose VID/PID
    /// match the bundle). Returns the user's choice; "Keep default" leaves the
    /// trigger unchanged, a device rebinds it via IFD_VID/IFD_PID.
    func pickUsbTrigger() -> TriggerChoice {
        let devices = usbDevices()
        let a = NSAlert()
        a.messageText = "Choose the USB trigger device"
        a.informativeText = "macOS loads the smart-card driver only while a USB device with a "
            + "matching ID is plugged in. Pick the device you'll keep attached as the trigger "
            + "(any USB stick works), or choose \u{201C}Keep default trigger\u{201D} to leave it "
            + "unchanged."
        a.addButton(withTitle: "Install")
        a.addButton(withTitle: "Cancel")
        let popup = NSPopUpButton(frame: NSRect(x: 0, y: 0, width: 340, height: 26))
        popup.addItem(withTitle: "Keep default trigger")
        for d in devices { popup.addItem(withTitle: "\(d.label)  —  \(d.vid):\(d.pid)") }
        a.accessoryView = popup
        a.window.initialFirstResponder = popup
        NSApp.activate(ignoringOtherApps: true)
        guard a.runModal() == .alertFirstButtonReturn else { return .cancel }
        let idx = popup.indexOfSelectedItem
        if idx <= 0 { return .keepDefault }
        let d = devices[idx - 1]
        return .device(vid: d.vid, pid: d.pid)
    }

    /// One-time privileged install of the smart-card IFD handler (the toggle only
    /// flips the server flag; the handler still has to be copied into the system
    /// drivers dir). Lets the user pick the USB trigger device from a popup
    /// (matching the CLI's select-usb-trigger.sh), passes it to the embedded
    /// installer as IFD_VID/IFD_PID, then runs it (it prompts for admin via its
    /// own GUI dialog).
    @objc func installSmartcardHandler() {
        func say(_ msg: String, _ info: String) {
            let a = NSAlert()
            a.messageText = msg
            a.informativeText = info
            a.addButton(withTitle: "OK")
            NSApp.activate(ignoringOtherApps: true)
            _ = a.runModal()
        }
        guard let app = locateServerApp() else {
            say("macrdp.app not found", "Install macrdp.app first, then run this again.")
            return
        }
        let installer = app.appendingPathComponent("Contents/Resources/install-ifd-handler.sh").path
        guard FileManager.default.fileExists(atPath: installer) else {
            say("Installer not found",
                "This macrdp.app build doesn't bundle the smart-card handler installer.")
            return
        }
        var env: [String: String] = [:]
        var picked: String?
        switch pickUsbTrigger() {
        case .cancel: return
        case .keepDefault: break
        case let .device(vid, pid):
            env["IFD_VID"] = vid
            env["IFD_PID"] = pid
            picked = "\(vid):\(pid)"
        }
        let out = run("/bin/bash", [installer], env: env)
        if out.code == 0 {
            let trigger = picked.map { "the chosen trigger device (\($0))" } ?? "the USB trigger device"
            say("Smart-card handler installed",
                "Unplug/replug \(trigger) so macOS loads the driver, and make sure "
                    + "the connecting client redirects its smart card.")
        } else {
            say("Install failed",
                out.stdout.isEmpty
                    ? "The installer exited with code \(out.code)." : String(out.stdout.suffix(800)))
        }
    }

    // Standard 16:9 virtual-display resolutions, highest 1440p; default 1920×1080.
    static let resolutions: [(Int, Int, String)] = [
        (1280, 720, "1280 × 720"),
        (1600, 900, "1600 × 900"),
        (1920, 1080, "1920 × 1080 (1080p)"),
        (2560, 1440, "2560 × 1440 (1440p)"),
    ]

    /// (bundle id, menu label) curated shortcuts for the Ctrl→Cmd exclude list —
    /// common editors with an embedded terminal that can't be auto-detected.
    /// Anything else is added via "Add an app…" (which reads the bundle id off the
    /// chosen .app, so no need to know it).
    static let remapExcludeApps: [(String, String)] = [
        ("com.microsoft.VSCode", "Visual Studio Code"),
        ("com.microsoft.VSCodeInsiders", "VS Code — Insiders"),
        ("com.todesktop.230313mzl4w4u92", "Cursor"),
    ]

    /// (config value, menu label) for the keyboard-layout picker. The empty
    /// value = no translation (positional keycodes / US ANSI). Values match the
    /// short names `--keyboard-layout` accepts.
    static let keyboardLayouts: [(String, String)] = [
        ("", "US / default (no translation)"),
        ("british", "British"),
        ("french", "French (AZERTY)"),
        ("german", "German (QWERTZ)"),
        ("swissgerman", "Swiss German"),
        ("spanish", "Spanish"),
        ("italian", "Italian"),
        ("portuguese", "Portuguese"),
        ("brazilian", "Portuguese (Brazil)"),
        ("dutch", "Dutch"),
        ("belgian", "Belgian"),
        ("swedish", "Swedish"),
        ("norwegian", "Norwegian"),
        ("danish", "Danish"),
        ("finnish", "Finnish"),
        ("russian", "Russian"),
        ("polish", "Polish"),
        ("czech", "Czech"),
        ("hungarian", "Hungarian"),
    ]

    /// Re-exec the agent so config.env changes take effect, if it's running.
    func applyIfRunning() {
        if agentState().pid != nil {
            _ = run("/bin/launchctl", ["kickstart", "-k", service])
        }
    }

    @objc func editConfig() { ensureConfigExists(); NSWorkspace.shared.open(configURL) }
    @objc func openLogs() {
        if !FileManager.default.fileExists(atPath: logURL.path) {
            FileManager.default.createFile(atPath: logURL.path, contents: Data())
        }
        NSWorkspace.shared.open(logURL)
    }
    @objc func openScreenRecording() {
        openURL("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")
    }
    @objc func openAccessibility() {
        openURL("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
    }
    @objc func quit() { NSApp.terminate(nil) }

    func openURL(_ s: String) { if let u = URL(string: s) { NSWorkspace.shared.open(u) } }

    // MARK: - config.env IO

    func readConfig() -> [String: String] {
        guard let text = try? String(contentsOf: configURL, encoding: .utf8) else { return [:] }
        var d: [String: String] = [:]
        for raw in text.split(separator: "\n") {
            let line = raw.trimmingCharacters(in: .whitespaces)
            if line.isEmpty || line.hasPrefix("#") { continue }
            guard let eq = line.firstIndex(of: "=") else { continue }
            let k = String(line[..<eq]).trimmingCharacters(in: .whitespaces)
            var v = String(line[line.index(after: eq)...]).trimmingCharacters(in: .whitespaces)
            v = v.trimmingCharacters(in: CharacterSet(charactersIn: "\""))
            d[k] = v
        }
        return d
    }

    func writeConfig(key: String, value: String) {
        ensureConfigExists()
        guard let text = try? String(contentsOf: configURL, encoding: .utf8) else { return }
        var lines = text.components(separatedBy: "\n")
        var found = false
        for (i, raw) in lines.enumerated() {
            let line = raw.trimmingCharacters(in: .whitespaces)
            if line.hasPrefix("#") { continue }
            guard let eq = line.firstIndex(of: "=") else { continue }
            let k = String(line[..<eq]).trimmingCharacters(in: .whitespaces)
            if k == key { lines[i] = "\(key)=\(value)"; found = true; break }
        }
        if !found { lines.append("\(key)=\(value)") }
        // Always end with exactly one trailing newline — a config file with no
        // final newline makes a downstream append concatenate onto the last
        // key (which silently corrupted VD_HEIGHT + a new key once).
        let body = lines.joined(separator: "\n")
        let out = body.hasSuffix("\n") ? body : body + "\n"
        try? out.write(to: configURL, atomically: true, encoding: .utf8)
    }

    func ensureConfigExists() {
        let fm = FileManager.default
        try? fm.createDirectory(at: configURL.deletingLastPathComponent(),
                                withIntermediateDirectories: true)
        if !fm.fileExists(atPath: configURL.path) {
            let defaults = """
            BIND="127.0.0.1:3390"
            USE_KEYCHAIN=1
            ENABLE_H264=0
            ENABLE_AAC=0
            HIDPI=0
            UNMINIMIZE=0
            APP_SWITCHER_HUD=0
            ALT_TAB_SWITCH=0
            ENABLE_DRIVE_REDIRECTION=0
            ENABLE_SMARTCARD_REDIRECTION=0
            VIRTUAL_DISPLAY=0
            PRIMARY_MODE=none
            VD_WIDTH=1920
            VD_HEIGHT=1080
            EXTRA_FLAGS=""

            """
            try? defaults.write(to: configURL, atomically: true, encoding: .utf8)
        }
    }

    // MARK: - Process helper

    func run(_ path: String, _ args: [String], env: [String: String]? = nil)
        -> (code: Int32, stdout: String) {
        let p = Process()
        p.executableURL = URL(fileURLWithPath: path)
        p.arguments = args
        if let env = env, !env.isEmpty {
            var merged = ProcessInfo.processInfo.environment
            for (k, v) in env { merged[k] = v }
            p.environment = merged
        }
        let pipe = Pipe()
        p.standardOutput = pipe
        p.standardError = pipe
        do { try p.run() } catch { return (-1, "") }
        let data = pipe.fileHandleForReading.readDataToEndOfFile()
        p.waitUntilExit()
        return (p.terminationStatus, String(data: data, encoding: .utf8) ?? "")
    }
}

// Headless entry for scripted/MDM deploy + testing (no GUI, no status bar).
let cliArgs = CommandLine.arguments
if cliArgs.contains("--install-agent") || cliArgs.contains("--print-paths") {
    exit(AppController().runHeadless(cliArgs))
}

let app = NSApplication.shared
let controller = AppController()
app.delegate = controller
app.run()
