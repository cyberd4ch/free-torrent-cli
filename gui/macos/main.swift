// ftc — a native window for the local web UI.
//
// This is the "double-click and get an app" path on macOS. It is deliberately
// thin: it starts `ftc serve` on a free loopback port, reads the URL the server
// prints, and shows that page in a WKWebView. The window, the menu, the dock
// icon and Cmd-Q are AppKit's; everything the app *does* is still the Rust
// binary, so there is no second implementation of anything.
//
// Compiled by install.sh (or the release workflow) into:
//   ftc.app/Contents/MacOS/ftc-gui
// with `ftc` copied alongside it.
import AppKit
import WebKit
import Darwin

final class AppDelegate: NSObject, NSApplicationDelegate {
    private var window: NSWindow!
    private var webView: WKWebView!
    private var server: Process?
    private var output = ""
    private var signalSources: [DispatchSourceSignal] = []

    func applicationDidFinishLaunching(_ notification: Notification) {
        buildMenu()
        trapSignals()

        let frame = NSRect(x: 0, y: 0, width: 1100, height: 760)
        window = NSWindow(
            contentRect: frame,
            styleMask: [.titled, .closable, .miniaturizable, .resizable],
            backing: .buffered,
            defer: false
        )
        window.title = "ftc"
        window.minSize = NSSize(width: 720, height: 480)
        window.center()

        webView = WKWebView(frame: frame, configuration: WKWebViewConfiguration())
        webView.setValue(false, forKey: "drawsBackground")
        window.contentView = webView
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)

        startServer()
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        true
    }

    func applicationWillTerminate(_ notification: Notification) {
        // The server is a child process; it must not outlive the window.
        stopServer()
    }

    /// Terminate the child and wait, with a SIGKILL backstop, so a server that
    /// ignores SIGTERM cannot keep a port open after the window is gone.
    private func stopServer() {
        guard let process = server, process.isRunning else { return }
        let pid = process.processIdentifier
        server = nil
        process.terminate()
        for _ in 0..<20 where process.isRunning {
            usleep(50_000)
        }
        if process.isRunning {
            kill(pid, SIGKILL)
        }
    }

    /// A logout, a Ctrl-C from the shell that launched the app or a plain `kill`
    /// never reaches `applicationWillTerminate` — AppKit only runs that for its
    /// own quit path — so a signal would orphan a live `ftc serve` on a port
    /// nobody knows about. Trap the three that matter and shut down the same way.
    private func trapSignals() {
        for sig in [SIGTERM, SIGINT, SIGHUP] {
            signal(sig, SIG_IGN)
            let source = DispatchSource.makeSignalSource(signal: sig, queue: .main)
            source.setEventHandler { [weak self] in
                self?.stopServer()
                exit(0)
            }
            source.resume()
            signalSources.append(source)
        }
    }

    private func buildMenu() {
        let main = NSMenu()
        let appItem = NSMenuItem()
        main.addItem(appItem)

        let appMenu = NSMenu()
        appMenu.addItem(
            withTitle: "About ftc",
            action: #selector(NSApplication.orderFrontStandardAboutPanel(_:)),
            keyEquivalent: ""
        )
        appMenu.addItem(.separator())
        appMenu.addItem(
            withTitle: "Quit ftc",
            action: #selector(NSApplication.terminate(_:)),
            keyEquivalent: "q"
        )
        appItem.submenu = appMenu

        let editItem = NSMenuItem()
        main.addItem(editItem)
        let editMenu = NSMenu(title: "Edit")
        editMenu.addItem(withTitle: "Cut", action: #selector(NSText.cut(_:)), keyEquivalent: "x")
        editMenu.addItem(withTitle: "Copy", action: #selector(NSText.copy(_:)), keyEquivalent: "c")
        editMenu.addItem(withTitle: "Paste", action: #selector(NSText.paste(_:)), keyEquivalent: "v")
        editMenu.addItem(withTitle: "Select All", action: #selector(NSText.selectAll(_:)), keyEquivalent: "a")
        editItem.submenu = editMenu

        NSApp.mainMenu = main
    }

    /// The binary that ships inside the same bundle, then PATH-adjacent fallbacks.
    private func binaryPath() -> String? {
        let fm = FileManager.default
        if let explicit = ProcessInfo.processInfo.environment["FTC_BIN"], !explicit.isEmpty {
            return explicit
        }
        if let executable = Bundle.main.executablePath {
            let beside = (executable as NSString).deletingLastPathComponent + "/ftc"
            if fm.isExecutableFile(atPath: beside) { return beside }
        }
        let home = NSHomeDirectory()
        for candidate in ["/usr/local/bin/ftc", "\(home)/.local/bin/ftc"]
        where fm.isExecutableFile(atPath: candidate) {
            return candidate
        }
        return nil
    }

    private func startServer() {
        guard let binary = binaryPath() else {
            showError("Could not find the ftc binary.\n\nExpected it next to the app or in ~/.local/bin.")
            return
        }

        let downloads = (NSHomeDirectory() as NSString).appendingPathComponent("Downloads")
        let process = Process()
        process.executableURL = URL(fileURLWithPath: binary)
        process.arguments = ["serve", "--port", "0", "--bind", "127.0.0.1", "--output", downloads]
        process.currentDirectoryURL = URL(fileURLWithPath: NSHomeDirectory())

        let pipe = Pipe()
        process.standardOutput = pipe
        process.standardError = pipe
        server = process

        pipe.fileHandleForReading.readabilityHandler = { [weak self] handle in
            let data = handle.availableData
            guard let self, !data.isEmpty, let text = String(data: data, encoding: .utf8) else { return }
            self.output += text
            if let url = self.firstURL(in: self.output) {
                self.output = ""
                DispatchQueue.main.async {
                    self.webView.load(URLRequest(url: url))
                }
            }
        }

        do {
            try process.run()
        } catch {
            showError("Could not start ftc: \(error.localizedDescription)")
            return
        }

        // A server that dies or never prints a URL must not leave a blank window.
        DispatchQueue.main.asyncAfter(deadline: .now() + 15) { [weak self] in
            guard let self, let url = self.firstURL(in: self.output) else {
                self?.showError("The ftc server did not start.\n\n\(self?.output ?? "")")
                return
            }
            _ = url
        }
    }

    private func firstURL(in text: String) -> URL? {
        for line in text.split(separator: "\n") where line.contains("ftc web ui ->") {
            let parts = line.split(separator: " ")
            if let last = parts.last, let url = URL(string: String(last)) { return url }
        }
        return nil
    }

    private func showError(_ message: String) {
        DispatchQueue.main.async {
            let alert = NSAlert()
            alert.messageText = "ftc could not start"
            alert.informativeText = message
            alert.alertStyle = .warning
            alert.addButton(withTitle: "Quit")
            alert.runModal()
            NSApp.terminate(nil)
        }
    }
}

let delegate = AppDelegate()
let app = NSApplication.shared
app.setActivationPolicy(.regular)
app.delegate = delegate
app.run()
