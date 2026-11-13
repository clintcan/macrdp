import AppKit

// A one-time welcome on the controller's first launch. The controller is a
// menu-bar app with no window and no Dock icon (LSUIElement + .accessory), so a
// new user who opens it sees nothing happen unless they spot the small icon
// near the clock, which a full menu bar or a MacBook's notch can also hide.
// (Reported in #236.)

enum FirstLaunch {
    /// UserDefaults key recording that the welcome was shown (or wasn't needed).
    static let shownKey = "didShowFirstLaunchWelcome"

    /// Show the welcome only if it was never shown and the server isn't set up
    /// yet. Someone upgrading, whose LaunchAgent is already installed, already
    /// knows where the controller lives.
    static func shouldShow(alreadyShown: Bool, agentInstalled: Bool) -> Bool {
        !alreadyShown && !agentInstalled
    }
}

extension AppController {
    /// Runs once, after the status item exists (so "Show Menu" can open it).
    func showFirstLaunchWelcomeIfNeeded() {
        let defaults = UserDefaults.standard
        let installed = agentState().loaded
        let show = FirstLaunch.shouldShow(
            alreadyShown: defaults.bool(forKey: FirstLaunch.shownKey),
            agentInstalled: installed
        )
        // Recorded before the alert, and also when the agent is already
        // installed, so the welcome never comes back.
        if show || installed {
            defaults.set(true, forKey: FirstLaunch.shownKey)
        }
        guard show else { return }

        let a = NSAlert()
        a.alertStyle = .informational
        a.messageText = "macrdp Controller runs in the menu bar"
        a.informativeText = """
            Look for the display icon (a small monitor) near the clock. Click it and \
            choose “Start (first run sets up)” to install and start the RDP server.

            If you can't see the icon, your menu bar may be full: on a MacBook with \
            a notch, icons can hide behind it.
            """
        a.addButton(withTitle: "Start Now")
        a.addButton(withTitle: "Show Menu")
        a.addButton(withTitle: "Close")
        NSApp.activate(ignoringOtherApps: true)
        switch a.runModal() {
        case .alertFirstButtonReturn:
            start()
        case .alertSecondButtonReturn:
            statusItem.button?.performClick(nil)
        default:
            break
        }
    }
}
