import AppKit
import SwiftUI

/// `macrdptray --render-screenshots <dir>` draws each settings section to
/// `<dir>/settings-<section>.png` for the user manual (docs/user-manual.md).
///
/// The views are rendered offscreen from a fixed sample configuration, so the
/// images don't depend on this Mac's settings, need no Screen Recording
/// permission, and can be regenerated after any UI change. Nothing is read
/// from or written to config.env. The Status and Permissions sections still
/// show this Mac's live state (server stopped, no client), as they would.
/// AppKit draws a switch grey in a window that isn't key, and an offscreen
/// window never is; this one says it is, so the images show the colours a
/// user sees.
private final class KeyLookingWindow: NSWindow {
    override var isKeyWindow: Bool { true }
    override var isMainWindow: Bool { true }
}

enum ScreenshotRenderer {
    /// A typical setup: a virtual display, H.264, and the redirections on.
    static let sample: [String: String] = [
        "BIND": "0.0.0.0:3390",
        "USE_KEYCHAIN": "1",
        "ENABLE_H264": "1",
        "ADAPTIVE_BITRATE": "1",
        "ENABLE_AAC": "1",
        "NETWORK_AUTODETECT": "1",
        "VIRTUAL_DISPLAY": "1",
        "VD_WIDTH": "1920",
        "VD_HEIGHT": "1080",
        "VD_HIDPI": "auto",
        "PRIMARY_MODE": "none",
        "MAIN_DISPLAY_WHILE_CONNECTED": "1",
        "RESTORE_WINDOWS_ON_DISCONNECT": "1",
        "ALT_TAB_SWITCH": "1",
        "APP_SWITCHER_HUD": "1",
        "ENABLE_DRIVE_REDIRECTION": "1",
        "ENABLE_MICROPHONE_REDIRECTION": "1",
        "ENABLE_CAMERA_REDIRECTION": "1",
    ]

    /// Height to draw each section at: tall enough for the longest forms.
    static func height(for section: SettingsSection) -> CGFloat {
        switch section {
        case .media: return 1300
        case .advanced: return 1420
        case .display: return 960
        case .connection: return 620
        default: return 560
        }
    }

    static func run(dir: String, controller: AppController) -> Int32 {
        let out = URL(fileURLWithPath: dir, isDirectory: true)
        try? FileManager.default.createDirectory(at: out, withIntermediateDirectories: true)
        let model = SettingsModel(controller: controller, sample: sample)
        for section in SettingsSection.allCases {
            model.section = section
            let h = height(for: section)
            // Drawn as in a key window: an offscreen window is inactive, and
            // macOS would grey out the switches.
            let root = SettingsView(model: model, height: h)
                .environment(\.colorScheme, .light)
                .environment(\.controlActiveState, .key)
            let host = NSHostingView(rootView: root)
            host.frame = NSRect(x: 0, y: 0, width: 660, height: h)
            // An offscreen window gives the view a backing scale (Retina) and
            // the light appearance; it is never shown.
            let window = KeyLookingWindow(contentRect: host.frame, styleMask: [.borderless],
                                          backing: .buffered, defer: false)
            window.appearance = NSAppearance(named: .aqua)
            window.contentView = host
            host.layoutSubtreeIfNeeded()
            // Let SwiftUI lay out and the Status section's first refresh land,
            // then lay out again: rows whose text wraps get their final height
            // on the second pass.
            RunLoop.current.run(until: Date().addingTimeInterval(1.0))
            host.needsLayout = true
            host.layoutSubtreeIfNeeded()
            RunLoop.current.run(until: Date().addingTimeInterval(0.5))
            guard let rep = host.bitmapImageRepForCachingDisplay(in: host.bounds) else {
                FileHandle.standardError.write("can't render \(section.rawValue)\n".data(using: .utf8)!)
                return 1
            }
            host.cacheDisplay(in: host.bounds, to: rep)
            let file = out.appendingPathComponent("settings-\(section.rawValue).png")
            guard let png = rep.representation(using: .png, properties: [:]),
                  (try? png.write(to: file)) != nil else {
                FileHandle.standardError.write("can't write \(file.path)\n".data(using: .utf8)!)
                return 1
            }
            print(file.path)
        }
        return 0
    }
}
