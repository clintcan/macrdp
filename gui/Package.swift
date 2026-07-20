// swift-tools-version:5.9
import PackageDescription

// macrdp Controller — a menu-bar (tray) front-end that drives the macrdp
// LaunchAgent and config.env set up by packaging/. Built with plain SwiftPM
// (no .xcodeproj) so it stays CLI-buildable; make-tray-app.sh wraps the
// resulting executable into macrdpController.app.
// `macrdphud` is a second executable: the app-switcher HUD overlay helper that
// macrdp spawns and drives over loopback to draw a visual Cmd+Tab switcher the
// remote client sees. make-hud-helper.sh embeds it inside macrdp.app.
// `macrdpshield` is a fourth executable: the black shield-window helper for
// --shield-primary, the headless blanking mode that (unlike --capture-primary)
// leaves the Mac lockable and survives a live re-mode without a desktop flash.
// make-shield-helper.sh embeds it inside macrdp.app.
// `macrdpcamera` is a third executable: the CoreMediaIO Camera **system
// extension** (camera redirection Phase 3) that presents the redirected webcam
// as a selectable macOS camera. It's assembled into a `.systemextension` bundle
// by packaging/make-camera-extension.sh and embedded in
// macrdp.app/Contents/Library/SystemExtensions/. Phase 3a emits a static test
// pattern; Phase 3b feeds it the real decoded frames via the sink stream.
let package = Package(
    name: "macrdptray",
    platforms: [.macOS(.v13)],
    targets: [
        .executableTarget(
            name: "macrdptray",
            path: "Sources/macrdptray",
            linkerSettings: [.linkedFramework("SystemExtensions")]
        ),
        .executableTarget(name: "macrdphud", path: "Sources/macrdphud"),
        .executableTarget(name: "macrdpshield", path: "Sources/macrdpshield"),
        .executableTarget(
            name: "macrdpcamera",
            path: "Sources/macrdpcamera",
            linkerSettings: [
                .linkedFramework("CoreMediaIO"),
                .linkedFramework("Security"),
            ]
        ),
    ]
)
