import AppKit
import Foundation
import PitboardKit

/// The `pitboard` a terminal runs, and a way to put this app's own there.
///
/// The app carries the command line inside it, and the cask for the app links that onto the
/// `PATH`. A copy downloaded from a release has nothing to do that, so the settings offer
/// to, the way editors on macOS put their own command there: one link in `/usr/local/bin`,
/// made after macOS asks for an administrator's password.
struct CommandLineTool: Sendable {
    /// This app's own command line, or nil when the app is not running from its bundle.
    let helper: String?
    /// Where each way of installing Pitboard puts it, looked in after the login shell's
    /// `PATH`, as the core says: cargo, a copy from a release, Homebrew on either kind of
    /// Mac, and the link made here.
    let installPlaces: [String]
    /// Where the link goes: `/usr/local/bin/pitboard` unless a test says otherwise, on the
    /// `PATH` macOS gives every shell and in a directory only an administrator can write to.
    let link: String
    /// Nonsecret paths the helper needs to read the same Desktop state as this app.
    let codeEnvironment: [String: String]
    /// A test hands in its own, so nothing it does waits on a password prompt.
    private let execute: Runner

    /// Runs an AppleScript and hands back the error it raised, or nil.
    typealias Runner = @Sendable (String) -> NSDictionary?

    init(
        helper: String?, installPlaces: [String], link: String,
        codeEnvironment: [String: String] = [:], execute: @escaping Runner
    ) {
        self.helper = helper
        self.installPlaces = installPlaces
        self.link = link
        self.codeEnvironment = codeEnvironment.filter {
            Self.codeEnvironmentKeys.contains($0.key)
        }
        self.execute = execute
    }

    /// This app's, looking where a person's installs go under `home`, which is the home the
    /// core reads from this app's environment unless a test says otherwise.
    init(
        bundle: URL = Bundle.main.bundleURL,
        home: String? = nil,
        link: String = "/usr/local/bin/pitboard",
        environment: [String: String] = ProcessInfo.processInfo.environment,
        execute: @escaping Runner = CommandLineTool.execute(script:)
    ) {
        let home = home ?? homeDirectory(environment: environment)
        var codeEnvironment = [
            "HOME": home,
            "PITBOARD_HOME": environment["PITBOARD_HOME"].map(Self.fromWorkingDirectory)
                ?? "\(home)/.pitboard",
            "PITBOARD_CLAUDE_DESKTOP_DIR": environment["PITBOARD_CLAUDE_DESKTOP_DIR"]
                .map(Self.fromWorkingDirectory) ?? "\(home)/Library/Application Support/Claude",
        ]
        // A bare name is looked up on the path, wherever it runs; one with a directory in it
        // is relative to this process.
        codeEnvironment["PITBOARD_CLAUDE"] = environment["PITBOARD_CLAUDE"].map {
            $0.contains("/") ? Self.fromWorkingDirectory($0) : $0
        }
        self.init(
            helper: Settings.bundledCommandLine(in: bundle),
            installPlaces: commandLinePlaces(home: home), link: link,
            codeEnvironment: codeEnvironment, execute: execute)
    }

    /// A relative path as this app's core reads it, from this process's working directory.
    /// Terminal starts the helper somewhere else, so it is handed the absolute one.
    private static func fromWorkingDirectory(_ path: String) -> String {
        path.isEmpty || path.hasPrefix("/") ? path : URL(fileURLWithPath: path).path
    }

    /// The first `pitboard` found.
    enum Found: Equatable, Sendable {
        /// This app's own, at this path or linked to from it.
        case bundled(String)
        /// Another install, at this path.
        case another(String)
        case nowhere
    }

    /// What linking came to.
    enum Linked: Equatable, Sendable {
        case linked
        /// The password prompt was dismissed, which is an answer and not a failure.
        case cancelled
        case failed(String)
    }

    /// The `pitboard` a terminal would run, found where it would find one: on the login
    /// shell's `PATH`, nil when the shell could not be asked, and then where each way of
    /// installing Pitboard puts it; and whether it is this app's own once every link on the
    /// way to it is followed. The core looks, on the file system, so not on the main thread.
    func find(onPath path: String?) -> Found {
        switch findCommandLine(searchPath: path, places: installPlaces, helper: helper) {
        case .bundled(let found): .bundled(found)
        case .another(let found): .another(found)
        case .nowhere: .nowhere
        }
    }

    /// macOS runs an app opened where it was downloaded from a temporary copy until it is
    /// moved, and a link into that copy stops working once the app quits.
    var translocated: Bool { helper?.contains("/AppTranslocation/") ?? false }

    /// Whether there is a command line in this app that a link would keep reaching. A build
    /// run from Xcode has no command line inside it, and a link to where one would be would
    /// cost an administrator's password for a link that runs nothing. Whether the one inside
    /// can run is the core's to say, as it says of every program it finds.
    var linkable: Bool {
        guard let helper, !translocated else { return false }
        return canRun(path: helper)
    }

    /// Links `link` to this app's command line once macOS has asked for an administrator's
    /// password. Off the main thread, since the prompt waits on a person. Anything at `link`
    /// that is not a link is somebody's own, and is left where it is.
    func install() async -> Linked {
        guard let helper, linkable else {
            return .failed("This copy of Pitboard cannot link the command line inside it.")
        }
        let type = try? FileManager.default.attributesOfItem(atPath: link)[.type]
        if let type, type as? FileAttributeType != .typeSymbolicLink {
            return .failed("\(link) is already there and is not a link, so it was kept.")
        }
        let (source, execute) = (Self.script(linking: helper, at: link), execute)
        return await Task.detached(priority: .userInitiated) {
            Self.outcome(of: execute(source))
        }.value
    }

    /// Opens Code through this app's helper. Terminal receives a label and nonsecret paths;
    /// the helper reads and validates the Desktop login without handing it back to the app.
    func openDesktopCode(label: String) async -> Linked {
        guard let helper, canRun(path: helper) else {
            return .failed(
                "This copy of Pitboard cannot open Claude Code with the command line inside it."
            )
        }
        let (source, execute) = (
            Self.script(openingDesktopCode: helper, label: label, environment: codeEnvironment),
            execute
        )
        return await Task.detached(priority: .userInitiated) {
            Self.outcome(of: execute(source))
        }.value
    }

    private static let codeEnvironmentKeys = [
        "HOME", "PITBOARD_HOME", "PITBOARD_CLAUDE_DESKTOP_DIR", "PITBOARD_CLAUDE",
    ]

    /// Terminal runs the bundled helper with shell-quoted arguments, never a login token.
    static func script(
        openingDesktopCode helper: String, label: String, environment: [String: String] = [:]
    ) -> String {
        let assignments = codeEnvironmentKeys.compactMap { key in
            environment[key].map { "quoted form of \(literal("\(key)=\($0)")) & \" \" & " }
        }.joined()
        return "tell application \"Terminal\"\n"
            + "    do script \"/usr/bin/env \" & \(assignments)"
            + "quoted form of \(literal(helper)) & \" desktop code -- \" & "
            + "quoted form of \(literal(label))\n"
            + "    activate\nend tell"
    }

    /// The script that runs `command` as an administrator. macOS asks for the password in
    /// this app's name before anything runs.
    static func script(linking helper: String, at link: String) -> String {
        "do shell script \(command(linking: helper, at: link)) with administrator privileges"
    }

    /// The shell command that makes the link, as an AppleScript expression. Each path is an
    /// AppleScript string handed to the shell through `quoted form of`, so a quote in a path
    /// cannot end either early, and nothing in one is read by the shell as a command.
    static func command(linking helper: String, at link: String) -> String {
        let directory = (link as NSString).deletingLastPathComponent
        return "\"mkdir -p \" & quoted form of \(literal(directory)) & \" && ln -sfh \" & "
            + "quoted form of \(literal(helper)) & \" \" & quoted form of \(literal(link))"
    }

    /// `text` as an AppleScript string: a backslash and a double quote are the only
    /// characters one cannot hold as they are.
    static func literal(_ text: String) -> String {
        let escaped = text.replacingOccurrences(of: "\\", with: "\\\\")
            .replacingOccurrences(of: "\"", with: "\\\"")
        return "\"\(escaped)\""
    }

    /// Runs `source` as an AppleScript, and hands back the error it raised, or nil.
    static func execute(script source: String) -> NSDictionary? {
        guard let script = NSAppleScript(source: source) else {
            return [NSAppleScript.errorMessage: "The link could not be made."]
        }
        var error: NSDictionary?
        script.executeAndReturnError(&error)
        return error
    }

    /// What running the script came to, from the error it raised. A dismissed password
    /// prompt raises `userCanceledErr`, which is an answer and not a failure.
    static func outcome(of error: NSDictionary?) -> Linked {
        guard let error else { return .linked }
        if error[NSAppleScript.errorNumber] as? Int == userCanceledErr { return .cancelled }
        return .failed(
            error[NSAppleScript.errorMessage] as? String ?? "The link could not be made.")
    }
}
