// bsnfs.app: a menu-bar app around the bsnfs server.
//
// The app runs the bundled command-line server (Contents/MacOS/bsnfs-cli) as
// a child process and gives it a menu: status, the admin page, restart, the
// log, and Start at Login. Running the server as the app's child means macOS
// names "bsnfs" (not Terminal) when it asks for access to protected folders.
//
// The server is started with --watch-stdin and the app holds its input open,
// so the server exits with the app even if the app is force-quit.
//
// Settings: shares added on the admin page are saved by the server in
// ~/Library/Application Support/bsnfs/exports. If
// ~/Library/Application Support/bsnfs/bsnfs.conf exists, it's passed to the
// server too (any bsnfs option, one per line). The server's output goes to
// ~/Library/Logs/bsnfs.log.

import AppKit
import ServiceManagement

final class AppDelegate: NSObject, NSApplicationDelegate, NSMenuDelegate {
    private var statusItem: NSStatusItem!
    private let menu = NSMenu()
    private let statusLine = NSMenuItem(title: "Starting…", action: nil, keyEquivalent: "")
    private let detailLine = NSMenuItem(title: "", action: nil, keyEquivalent: "")
    private var loginItem: NSMenuItem?

    private var server: Process?
    private var serverInput: Pipe?
    private var adminURL = URL(string: "http://127.0.0.1:8080/")!
    private var running = false
    private var stopping = false
    private var restartWhenStopped = false
    private var openAdminWhenReady = false
    private var lastError: String?
    private var partialLine = ""
    private var log: FileHandle?

    private let files = FileManager.default
    private lazy var supportDir = files.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        .appendingPathComponent("bsnfs", isDirectory: true)
    private lazy var logURL = files.urls(for: .libraryDirectory, in: .userDomainMask)[0]
        .appendingPathComponent("Logs/bsnfs.log")

    func applicationDidFinishLaunching(_ notification: Notification) {
        statusItem = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
        buildMenu()
        statusItem.menu = menu
        // First run, with nothing saved yet: open the admin page so there's
        // somewhere to add a folder.
        openAdminWhenReady = !files.fileExists(atPath: supportDir.appendingPathComponent("exports").path)
        startServer()
    }

    func applicationWillTerminate(_ notification: Notification) {
        stopping = true
        guard let server = server, server.isRunning else { return }
        server.terminate() // SIGTERM: the server unregisters and exits
        let deadline = Date().addingTimeInterval(3)
        while server.isRunning && Date() < deadline {
            RunLoop.current.run(until: Date().addingTimeInterval(0.05))
        }
    }

    // MARK: Menu

    private func buildMenu() {
        menu.delegate = self
        statusLine.isEnabled = false
        detailLine.isEnabled = false
        detailLine.isHidden = true
        menu.addItem(statusLine)
        menu.addItem(detailLine)
        menu.addItem(.separator())
        menu.addItem(action("Open Admin Page", #selector(openAdmin), "o"))
        menu.addItem(action("Restart Server", #selector(restart), "r"))
        menu.addItem(action("Show Log", #selector(showLog), "l"))
        menu.addItem(.separator())
        if #available(macOS 13.0, *) {
            let item = action("Start at Login", #selector(toggleStartAtLogin), "")
            menu.addItem(item)
            loginItem = item
        }
        let version = Bundle.main.object(forInfoDictionaryKey: "BSNFSVersion") as? String ?? "dev"
        let about = NSMenuItem(title: "bsnfs \(version)", action: nil, keyEquivalent: "")
        about.isEnabled = false
        menu.addItem(about)
        menu.addItem(action("Quit bsnfs", #selector(quit), "q"))
        refresh()
    }

    private func action(_ title: String, _ selector: Selector, _ key: String) -> NSMenuItem {
        let item = NSMenuItem(title: title, action: selector, keyEquivalent: key)
        item.target = self
        return item
    }

    func menuWillOpen(_ menu: NSMenu) {
        refresh()
        fetchStatus()
    }

    /// Bring the icon, status line and checkmarks up to date.
    private func refresh() {
        if running {
            statusLine.title = "bsnfs is running"
        } else if let error = lastError {
            statusLine.title = error.count > 90 ? String(error.prefix(89)) + "…" : error
            statusLine.toolTip = error
        } else {
            statusLine.title = "Starting…"
        }
        if !running { detailLine.isHidden = true }
        let names = running
            ? ["externaldrive.connected.to.line.below", "externaldrive"]
            : ["externaldrive.badge.exclamationmark", "externaldrive.badge.xmark", "externaldrive"]
        if let image = names.lazy.compactMap({ NSImage(systemSymbolName: $0, accessibilityDescription: "bsnfs") }).first {
            image.isTemplate = true
            statusItem.button?.image = image
            statusItem.button?.title = ""
        } else {
            statusItem.button?.title = "NFS"
        }
        statusItem.button?.appearsDisabled = !running
        if #available(macOS 13.0, *) {
            loginItem?.state = SMAppService.mainApp.status == .enabled ? .on : .off
        }
    }

    /// Ask the server how many folders it shares and who has them mounted.
    private func fetchStatus() {
        guard running else { return }
        var request = URLRequest(url: adminURL.appendingPathComponent("api/status"))
        request.timeoutInterval = 2
        URLSession.shared.dataTask(with: request) { [weak self] data, response, _ in
            guard let data = data, (response as? HTTPURLResponse)?.statusCode == 200,
                  let status = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] else { return }
            let shares = (status["exports"] as? [Any])?.count ?? 0
            let mounts = (status["mounts"] as? [Any])?.count ?? 0
            var text = shares == 1 ? "Sharing 1 folder" : "Sharing \(shares) folders"
            if mounts > 0 { text += mounts == 1 ? ", 1 mount" : ", \(mounts) mounts" }
            DispatchQueue.main.async {
                guard let self = self, self.running else { return }
                self.detailLine.title = text
                self.detailLine.isHidden = false
            }
        }.resume()
    }

    // MARK: Actions

    @objc private func openAdmin() {
        NSWorkspace.shared.open(adminURL)
    }

    @objc private func restart() {
        if let server = server, server.isRunning {
            restartWhenStopped = true
            stopping = true
            server.terminate()
        } else {
            startServer()
        }
    }

    @objc private func showLog() {
        NSWorkspace.shared.open(logURL)
    }

    @available(macOS 13.0, *)
    @objc private func toggleStartAtLogin() {
        let service = SMAppService.mainApp
        do {
            if service.status == .enabled {
                try service.unregister()
            } else {
                try service.register()
                if service.status == .requiresApproval {
                    SMAppService.openSystemSettingsLoginItems()
                }
            }
        } catch {
            let alert = NSAlert()
            alert.messageText = "Couldn't change Start at Login"
            alert.informativeText = error.localizedDescription
            alert.runModal()
        }
        refresh()
    }

    @objc private func quit() {
        NSApp.terminate(nil)
    }

    // MARK: The server

    private func startServer() {
        guard server == nil else { return }
        let cli = Bundle.main.bundleURL.appendingPathComponent("Contents/MacOS/bsnfs-cli")
        var args = ["--watch-stdin"]
        let config = supportDir.appendingPathComponent("bsnfs.conf")
        if files.fileExists(atPath: config.path) {
            args += ["--config", config.path]
        }
        // For development: extra options, e.g. other ports.
        if let extra = ProcessInfo.processInfo.environment["BSNFS_APP_ARGS"] {
            args += extra.split(separator: " ").map(String.init)
        }

        let process = Process()
        process.executableURL = cli
        process.arguments = args
        process.currentDirectoryURL = files.homeDirectoryForCurrentUser
        let input = Pipe()
        let output = Pipe()
        process.standardInput = input
        process.standardOutput = output
        process.standardError = output
        output.fileHandleForReading.readabilityHandler = { [weak self] handle in
            let data = handle.availableData
            if data.isEmpty { handle.readabilityHandler = nil } // end of output
            DispatchQueue.main.async { self?.consume(data) }
        }
        process.terminationHandler = { [weak self] finished in
            DispatchQueue.main.async { self?.serverExited(finished.terminationStatus) }
        }

        openLog()
        lastError = nil
        running = false
        stopping = false
        do {
            try process.run()
        } catch {
            lastError = "Couldn't start the server: \(error.localizedDescription)"
            refresh()
            return
        }
        server = process
        serverInput = input
        refresh()
    }

    /// The server's output: copy it to the log and watch for its startup
    /// banner and errors.
    private func consume(_ data: Data) {
        guard !data.isEmpty else { return }
        log?.write(data)
        partialLine += String(decoding: data, as: UTF8.self)
        while let newline = partialLine.firstIndex(of: "\n") {
            let line = String(partialLine[..<newline])
            partialLine.removeSubrange(...newline)
            handle(line)
        }
    }

    private func handle(_ line: String) {
        if let range = line.range(of: "admin page on ") {
            let rest = line[range.upperBound...].trimmingCharacters(in: .whitespaces)
            if let url = URL(string: rest) { adminURL = url }
        }
        if line.contains(" serving NFS ") {
            running = true
            refresh()
        }
        if line.contains("e.g.: mount") || line.contains("no folders shared yet") {
            // The banner is complete: the admin page is up.
            if openAdminWhenReady {
                openAdminWhenReady = false
                openAdmin()
            }
        }
        if line.hasPrefix("bsnfs: ") {
            let error = String(line.dropFirst("bsnfs: ".count))
            if error.contains("Address already in use"), let port = error.split(separator: ":").dropFirst().first {
                // The usual cause: another bsnfs, or macOS's own NFS server.
                lastError = "Port \(port) is in use (another bsnfs, or macOS's NFS server?)"
            } else {
                lastError = error
            }
        }
    }

    private func serverExited(_ status: Int32) {
        server = nil
        serverInput = nil
        running = false
        if restartWhenStopped {
            restartWhenStopped = false
            startServer()
            return
        }
        if !stopping && lastError == nil {
            lastError = "The server stopped (exit status \(status)). See the log."
        }
        refresh()
    }

    private func openLog() {
        guard log == nil else { return }
        try? files.createDirectory(at: logURL.deletingLastPathComponent(), withIntermediateDirectories: true)
        // Keep one old log once it passes 5 MB.
        if let size = (try? files.attributesOfItem(atPath: logURL.path))?[.size] as? Int, size > 5_000_000 {
            let old = logURL.deletingPathExtension().appendingPathExtension("old.log")
            try? files.removeItem(at: old)
            try? files.moveItem(at: logURL, to: old)
        }
        if !files.fileExists(atPath: logURL.path) {
            files.createFile(atPath: logURL.path, contents: nil)
        }
        log = try? FileHandle(forWritingTo: logURL)
        log?.seekToEndOfFile()
    }
}

let app = NSApplication.shared
let delegate = AppDelegate()
app.delegate = delegate
app.setActivationPolicy(.accessory)
app.run()
