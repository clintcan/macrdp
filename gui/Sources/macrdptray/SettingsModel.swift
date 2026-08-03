import AppKit
import SwiftUI

// Draft model backing the tabbed Settings window (SettingsWindow.swift).
//
// Everything the old menu toggled lands in config.env as KEY=value, applied by
// re-exec'ing the LaunchAgent (`launchctl kickstart -k`). The menu wrote + kick-
// started on EVERY click (one server restart per toggle). This model instead
// loads config.env into an in-memory `draft`, lets the UI mutate the draft with
// NO disk write and NO restart, and applies everything at once on Apply (write
// the changed keys, then a single kickstart). Revert reloads from disk.
//
// Imperative actions (password, smart-card installer, camera extension, log/pane
// opens) are NOT part of the draft — they run immediately via the controller,
// exactly as before. Only declarative config lives in the draft.
final class SettingsModel: ObservableObject {
    unowned let controller: AppController

    /// Config exactly as last read from disk (all keys, GUI-managed or not).
    /// The draft compares against this verbatim; legacy-key back-compat (e.g.
    /// CAPTURE_PRIMARY) is handled by computed accessors, not by rewriting it.
    @Published private(set) var saved: [String: String]
    /// Working copy the UI edits; differs from `saved` exactly when dirty.
    @Published var draft: [String: String]
    /// Set after a successful Apply, for transient "Applied ✓" feedback.
    @Published var lastAppliedAt: Date?
    /// Cached at open + after Apply (NOT recomputed in the view body — it shells
    /// out to `launchctl`, which per-render would spawn a subprocess per keystroke).
    @Published private(set) var serverRunning = false
    /// Which section the sidebar shows. Hoisted here (not local @State) so the
    /// main-menu "Section" items can navigate the window too.
    @Published var section: SettingsSection = .status

    init(controller: AppController) {
        self.controller = controller
        controller.ensureConfigExists()
        let cfg = controller.readConfig()
        self.saved = cfg
        self.draft = cfg
        self.serverRunning = controller.agentState().pid != nil
    }

    /// True when the draft diverges from the on-disk config, i.e. Apply has work.
    var isDirty: Bool { draft != saved }

    func reload() {
        let cfg = controller.readConfig()
        saved = cfg
        draft = cfg
    }

    func revert() { draft = saved }

    /// Write every changed key, then kickstart once (if the server is running).
    func apply() {
        for (key, value) in draft where saved[key] != value {
            controller.writeConfig(key: key, value: value)
        }
        controller.applyIfRunning()
        reload()
        serverRunning = controller.agentState().pid != nil
        lastAppliedAt = Date()
    }

    // MARK: - Typed value access + SwiftUI bindings

    func bool(_ key: String, default def: Bool = false) -> Bool {
        (draft[key] ?? (def ? "1" : "0")) == "1"
    }

    func string(_ key: String, default def: String = "") -> String { draft[key] ?? def }

    func setBool(_ key: String, _ value: Bool) {
        draft[key] = value ? "1" : "0"
        normalize()
    }

    func setString(_ key: String, _ value: String) {
        draft[key] = value
        normalize()
    }

    func boolBinding(_ key: String, default def: Bool = false) -> Binding<Bool> {
        Binding(get: { self.bool(key, default: def) }, set: { self.setBool(key, $0) })
    }

    func stringBinding(_ key: String, default def: String = "") -> Binding<String> {
        Binding(get: { self.string(key, default: def) }, set: { self.setString(key, $0) })
    }

    // MARK: - Dependent-key constraints (mirror the old menu auto-enable logic)

    // Turning a PARENT off is always authoritative — it resets its dependent
    // child, never the reverse (a child requirement must not re-enable a parent
    // the user just switched off). Enabling a mode forces its parent on in the
    // setter that owns that intent (setPrimaryMode), not here. Each branch mutates
    // only when the value would actually change, so an unrelated toggle never
    // appends spurious keys, and a legacy CAPTURE_PRIMARY=1 config is left intact
    // until the user touches the display settings.
    private func normalize() {
        let mode = draft["PRIMARY_MODE"] ?? ""
        let modeActive = mode != "" && mode != "none"
        // Virtual display off => no primary-screen takeover (reset the mode +
        // retire the legacy capture flag). No "force VD on" branch: setPrimaryMode
        // already enables VD when a mode is chosen, so this can't fight a VD-off.
        if (draft["VIRTUAL_DISPLAY"] ?? "0") != "1" {
            if modeActive { draft["PRIMARY_MODE"] = "none" }
            if draft["CAPTURE_PRIMARY"] == "1" { draft["CAPTURE_PRIMARY"] = "0" }
        }
        // Tunnel off => clear the video-migrate child. Tunnel on + migrate on =>
        // ensure H.264 (the migrate toggle is UI-disabled until the tunnel is on,
        // so this never has to re-enable the tunnel itself).
        if (draft["ENABLE_UDP_MULTITRANSPORT"] ?? "0") != "1" {
            if draft["UDP_MIGRATE_EGFX"] == "1" { draft["UDP_MIGRATE_EGFX"] = "0" }
        } else if (draft["UDP_MIGRATE_EGFX"] ?? "0") == "1" {
            draft["ENABLE_H264"] = "1"
        }
    }

    // MARK: - Network bind (loopback <-> all interfaces, port preserved)

    var bindDisplay: String { string("BIND", default: "127.0.0.1:3390") }

    var allowNetwork: Bool { bindDisplay.hasPrefix("0.0.0.0") }

    func setAllowNetwork(_ on: Bool) {
        let port = bindDisplay.split(separator: ":").last.map(String.init) ?? "3390"
        setString("BIND", "\(on ? "0.0.0.0" : "127.0.0.1"):\(port)")
    }

    // MARK: - Primary-screen mode

    /// Effective mode, honoring a legacy `CAPTURE_PRIMARY=1` config that predates
    /// `PRIMARY_MODE` (so the picker reflects reality without rewriting the file).
    var primaryMode: String {
        let m = draft["PRIMARY_MODE"] ?? ""
        if !m.isEmpty { return m }
        return draft["CAPTURE_PRIMARY"] == "1" ? "capture" : "none"
    }

    /// Choosing a mode makes `PRIMARY_MODE` authoritative and retires the legacy
    /// `CAPTURE_PRIMARY` boolean, so the server never sees a conflicting pair.
    func setPrimaryMode(_ mode: String) {
        draft["PRIMARY_MODE"] = mode
        if mode != "none" { draft["VIRTUAL_DISPLAY"] = "1" }
        if draft["CAPTURE_PRIMARY"] == "1" { draft["CAPTURE_PRIMARY"] = "0" }
        normalize()
    }

    // MARK: - Virtual-display resolution

    var resolution: String {
        "\(string("VD_WIDTH", default: "1920"))x\(string("VD_HEIGHT", default: "1080"))"
    }
    func setResolution(_ wxh: String) {
        let parts = wxh.split(separator: "x")
        guard parts.count == 2 else { return }
        draft["VD_WIDTH"] = String(parts[0])
        draft["VD_HEIGHT"] = String(parts[1])
        normalize()
    }

    // MARK: - H.264 bitrate ceiling (Mbit/s)

    /// Effective ceiling: the BITRATE key if set, else a `--bitrate N` left in
    /// EXTRA_FLAGS (back-compat with hand-edited configs), else the default (6).
    var bitrateMbps: String {
        if let b = draft["BITRATE"], !b.isEmpty { return b }
        if let n = Self.extraFlagsBitrate(string("EXTRA_FLAGS")) { return n }
        return "6"
    }

    /// Set the ceiling via BITRATE and strip any `--bitrate N` from EXTRA_FLAGS so
    /// the two can never disagree (the server would otherwise take the last one).
    func setBitrate(_ mbps: String) {
        draft["BITRATE"] = mbps
        let extra = string("EXTRA_FLAGS")
        let cleaned = Self.stripBitrate(extra)
        if cleaned != extra { draft["EXTRA_FLAGS"] = cleaned }
        normalize()
    }

    private static func extraFlagsBitrate(_ extra: String) -> String? {
        let toks = extra.split(separator: " ").map(String.init)
        guard let i = toks.firstIndex(of: "--bitrate"), i + 1 < toks.count else { return nil }
        return toks[i + 1]
    }

    private static func stripBitrate(_ extra: String) -> String {
        var toks = extra.split(separator: " ").map(String.init)
        if let i = toks.firstIndex(of: "--bitrate") {
            toks.remove(at: i)                       // the flag
            if i < toks.count { toks.remove(at: i) } // its value
        }
        return toks.joined(separator: " ")
    }

    // MARK: - Ctrl->Cmd exclude list (NO_REMAP_APPS)

    func excludeList() -> [String] {
        string("NO_REMAP_APPS")
            .split(separator: ",")
            .map { $0.trimmingCharacters(in: .whitespaces) }
            .filter { !$0.isEmpty }
    }

    func setExcludeList(_ apps: [String]) {
        var seen = Set<String>()
        let deduped = apps.filter { !$0.isEmpty && seen.insert($0).inserted }
        setString("NO_REMAP_APPS", deduped.joined(separator: ","))
    }

    func isExcluded(_ bundle: String) -> Bool { excludeList().contains(bundle) }

    func toggleExclude(_ bundle: String, _ on: Bool) {
        var list = excludeList()
        if on {
            if !list.contains(bundle) { list.append(bundle) }
        } else {
            list.removeAll { $0 == bundle }
        }
        setExcludeList(list)
    }

    /// Pick any .app and add its bundle id to the exclude list (draft-only; the
    /// change applies with the next Apply). Reads the id off the bundle so the
    /// user never types it.
    func addExcludeApp() {
        let panel = NSOpenPanel()
        panel.title = "Choose an app to keep Ctrl unmapped in"
        panel.canChooseFiles = true
        panel.canChooseDirectories = false
        panel.allowsMultipleSelection = false
        panel.allowedContentTypes = [.application]
        panel.directoryURL = URL(fileURLWithPath: "/Applications")
        NSApp.activate(ignoringOtherApps: true)
        guard panel.runModal() == .OK, let url = panel.url,
              let bundle = Bundle(url: url)?.bundleIdentifier else { return }
        var list = excludeList()
        list.append(bundle)
        setExcludeList(list)
    }

    // MARK: - Remote-desktop preset (draft-only)

    /// Stage the recommended "remote into my Mac" config in the draft (headless
    /// virtual display + detach the physical panel + H.264 + app-switcher HUD).
    /// The user reviews and hits Apply; unlike the old menu preset it does not
    /// self-install/start — Start on the tray does that.
    func applyRemoteDesktopPreset() {
        draft["VIRTUAL_DISPLAY"] = "1"
        draft["PRIMARY_MODE"] = "detach"
        draft["ENABLE_H264"] = "1"
        draft["APP_SWITCHER_HUD"] = "1"
        if (draft["VD_WIDTH"] ?? "").isEmpty { draft["VD_WIDTH"] = "1920" }
        if (draft["VD_HEIGHT"] ?? "").isEmpty { draft["VD_HEIGHT"] = "1080" }
        normalize()
    }
}
