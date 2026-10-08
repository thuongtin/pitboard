import Foundation
import Testing

@testable import PitboardApp

private struct DesktopCodeHelper {
    let root = FileManager.default.temporaryDirectory
        .appendingPathComponent("pitboard-desktop-code-\(UUID().uuidString)")
    var path: String { root.appendingPathComponent("pitboard").path }

    init() throws {
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        try Data("#!/bin/sh\n".utf8).write(to: URL(fileURLWithPath: path))
        try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: path)
    }

    func tool(_ runner: @escaping CommandLineTool.Runner) -> CommandLineTool {
        CommandLineTool(helper: path, installPlaces: [], link: "", execute: runner)
    }

    func remove() { try? FileManager.default.removeItem(at: root) }
}

/// Both arguments stay AppleScript strings and are shell-quoted before Terminal sees them.
@Test func desktopCodeQuotesTheHelperAndLabel() {
    let helper = #"/tmp/it's "mine" \ $(echo injected)/pitboard"#
    let label = #"work's "account" \ $(echo injected); exit"#
    #expect(
        CommandLineTool.script(openingDesktopCode: helper, label: label)
            == #"tell application "Terminal""# + "\n"
            + #"    do script "/usr/bin/env " & quoted form of "/tmp/it's \"mine\" \\ $(echo injected)/pitboard" & " desktop code -- " & quoted form of "work's \"account\" \\ $(echo injected); exit""#
            + "\n    activate\nend tell")
}

/// Evaluating only the quoted expression never addresses or opens Terminal.
@MainActor
@Test func desktopCodeArgumentsReachTheShellAsLiteralText() throws {
    let helper = #"/tmp/it's "mine" \ $(echo injected)/pitboard"#
    let label = #"work's "account" \ $(echo injected); exit"#
    let source = CommandLineTool.script(openingDesktopCode: helper, label: label)
    let line = try #require(source.split(separator: "\n").dropFirst().first)
    let expression = line.dropFirst("    do script ".count)
    var error: NSDictionary?
    let result = NSAppleScript(source: "return \(expression)")?.executeAndReturnError(&error)
    #expect(error == nil)
    #expect(
        result?.stringValue
            == #"/usr/bin/env '/tmp/it'\''s "mine" \ $(echo injected)/pitboard' desktop code -- 'work'\''s "account" \ $(echo injected); exit'"#
    )
}

@Test func desktopCodeRunsOnlyTheBundledHelper() async throws {
    let helper = try DesktopCodeHelper()
    defer { helper.remove() }
    let scripts = Scripts()
    #expect(await helper.tool(scripts.run).openDesktopCode(label: "work") == .linked)
    #expect(
        scripts.ran == [CommandLineTool.script(openingDesktopCode: helper.path, label: "work")])
}

@Test func desktopCodeKeepsTheAppsLaunchContext() async throws {
    let helper = try DesktopCodeHelper()
    defer { helper.remove() }
    let scripts = Scripts()
    let environment = [
        "HOME": "/tmp/app home", "PITBOARD_HOME": "/tmp/app state",
        "PITBOARD_CLAUDE_DESKTOP_DIR": "/tmp/Desktop data",
        "PITBOARD_CLAUDE": "/tmp/Claude Code",
        "UNRELATED_SECRET": "never-forward",
    ]
    let tool = CommandLineTool(
        helper: helper.path, installPlaces: [], link: "", codeEnvironment: environment,
        execute: scripts.run)
    #expect(await tool.openDesktopCode(label: "--print") == .linked)
    #expect(
        scripts.ran == [
            CommandLineTool.script(
                openingDesktopCode: helper.path, label: "--print", environment: environment)
        ])
    #expect(!scripts.ran.joined().contains("never-forward"))
}

@Test func desktopCodeUsesTheAppsHomeAndExplicitOverrides() {
    let home = "/tmp/fixture home"
    let tool = CommandLineTool(home: home, environment: [:], execute: { _ in nil })
    #expect(
        tool.codeEnvironment == [
            "HOME": home, "PITBOARD_HOME": "\(home)/.pitboard",
            "PITBOARD_CLAUDE_DESKTOP_DIR": "\(home)/Library/Application Support/Claude",
        ])
    let overridden = CommandLineTool(
        home: home,
        environment: [
            "HOME": "/tmp/other", "PITBOARD_HOME": "/tmp/state",
            "PITBOARD_CLAUDE_DESKTOP_DIR": "/tmp/desktop", "PITBOARD_CLAUDE": "/tmp/code",
            "UNRELATED_SECRET": "never-forward",
        ], execute: { _ in nil })
    #expect(
        overridden.codeEnvironment == [
            "HOME": home, "PITBOARD_HOME": "/tmp/state",
            "PITBOARD_CLAUDE_DESKTOP_DIR": "/tmp/desktop", "PITBOARD_CLAUDE": "/tmp/code",
        ])
    let inferred = CommandLineTool(environment: ["HOME": home], execute: { _ in nil })
    #expect(inferred.codeEnvironment == tool.codeEnvironment)
}

/// Terminal starts the helper in another directory, so a path the app was given relatively is
/// handed over as the one the app's own core reads.
@Test func desktopCodeIsGivenAbsolutePathsForRelativeOverrides() {
    let tool = CommandLineTool(
        home: "/tmp/fixture home",
        environment: [
            "PITBOARD_HOME": "scratch/state", "PITBOARD_CLAUDE_DESKTOP_DIR": "scratch/data",
        ],
        execute: { _ in nil })
    let here = FileManager.default.currentDirectoryPath
    #expect(tool.codeEnvironment["PITBOARD_HOME"] == "\(here)/scratch/state")
    #expect(tool.codeEnvironment["PITBOARD_CLAUDE_DESKTOP_DIR"] == "\(here)/scratch/data")
}

/// The core reads an explicitly empty `PITBOARD_HOME` as the app's own directory, which is
/// not the one Terminal starts the helper in.
@Test func desktopCodeIsGivenTheAppsDirectoryForAnEmptyPitboardHome() {
    let tool = CommandLineTool(
        home: "/tmp/fixture home", environment: ["PITBOARD_HOME": ""], execute: { _ in nil })
    #expect(tool.codeEnvironment["PITBOARD_HOME"] == FileManager.default.currentDirectoryPath)
}

/// The core reads an explicitly empty `HOME` as the app's own directory too, so the paths
/// derived from it are that directory's, not the root's.
@Test func desktopCodeDerivesItsPathsFromTheAppsDirectoryForAnEmptyHome() {
    let here = FileManager.default.currentDirectoryPath
    let tool = CommandLineTool(environment: ["HOME": ""], execute: { _ in nil })
    #expect(tool.codeEnvironment["HOME"] == here)
    #expect(tool.codeEnvironment["PITBOARD_HOME"] == "\(here)/.pitboard")
    #expect(
        tool.codeEnvironment["PITBOARD_CLAUDE_DESKTOP_DIR"]
            == "\(here)/Library/Application Support/Claude")
}

/// A relative program with a directory in it is read from the app's directory, which the
/// helper Terminal starts does not share. A bare name is looked up on the path and stays.
@MainActor
@Test func desktopCodeIsGivenAbsoluteClaudeProgramOnlyWhenItNamesADirectory() {
    let here = FileManager.default.currentDirectoryPath
    let relative = CommandLineTool(
        home: "/tmp/fixture home", environment: ["PITBOARD_CLAUDE": "bin/claude"],
        execute: { _ in nil })
    #expect(relative.codeEnvironment["PITBOARD_CLAUDE"] == "\(here)/bin/claude")
    let bare = CommandLineTool(
        home: "/tmp/fixture home", environment: ["PITBOARD_CLAUDE": "claude"],
        execute: { _ in nil })
    #expect(bare.codeEnvironment["PITBOARD_CLAUDE"] == "claude")
}

/// Environment assignments are single arguments, even with shell metacharacters in paths.
@MainActor
@Test func desktopCodeQuotesOnlyAllowedEnvironmentArguments() throws {
    let source = CommandLineTool.script(
        openingDesktopCode: "/tmp/pitboard", label: "--print",
        environment: [
            "HOME": #"/tmp/it's "mine" \ $(echo injected)"#,
            "PITBOARD_HOME": "/tmp/state dir", "PITBOARD_CLAUDE_DESKTOP_DIR": "/tmp/data dir",
            "PITBOARD_CLAUDE": "/tmp/code tool", "UNRELATED_SECRET": "never-forward",
        ])
    let line = try #require(source.split(separator: "\n").dropFirst().first)
    let expression = line.dropFirst("    do script ".count)
    var error: NSDictionary?
    let result = NSAppleScript(source: "return \(expression)")?.executeAndReturnError(&error)
    #expect(error == nil)
    #expect(
        result?.stringValue
            == #"/usr/bin/env 'HOME=/tmp/it'\''s "mine" \ $(echo injected)' 'PITBOARD_HOME=/tmp/state dir' 'PITBOARD_CLAUDE_DESKTOP_DIR=/tmp/data dir' 'PITBOARD_CLAUDE=/tmp/code tool' '/tmp/pitboard' desktop code -- '--print'"#
    )
}

@Test func desktopCodeRequiresAnExecutableBundledHelper() async throws {
    let helper = try DesktopCodeHelper()
    defer { helper.remove() }
    let scripts = Scripts()
    let absent = CommandLineTool(helper: nil, installPlaces: [], link: "", execute: scripts.run)
    let failure = CommandLineTool.Linked.failed(
        "This copy of Pitboard cannot open Claude Code with the command line inside it.")
    #expect(await absent.openDesktopCode(label: "work") == failure)
    try FileManager.default.setAttributes([.posixPermissions: 0o644], ofItemAtPath: helper.path)
    #expect(await helper.tool(scripts.run).openDesktopCode(label: "work") == failure)
    try FileManager.default.removeItem(atPath: helper.path)
    #expect(await helper.tool(scripts.run).openDesktopCode(label: "work") == failure)
    #expect(scripts.ran.isEmpty)
}

@Test func desktopCodePreservesTheRunnerFailure() async throws {
    let helper = try DesktopCodeHelper()
    defer { helper.remove() }
    let tool = helper.tool { _ in [NSAppleScript.errorMessage: "Terminal refused the command."]
    }
    #expect(
        await tool.openDesktopCode(label: "work") == .failed("Terminal refused the command."))
}

/// The shipped app can ask for Terminal automation after both Xcode and release signing.
@Test func desktopCodeHasTheRequiredAutomationConfiguration() throws {
    let macos = URL(fileURLWithPath: #filePath)
        .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
    func plist(_ name: String) throws -> [String: Any] {
        let data = try Data(contentsOf: macos.appending(path: name))
        return try #require(
            PropertyListSerialization.propertyList(from: data, format: nil) as? [String: Any])
    }
    let info = try plist("App/Info.plist")
    #expect(
        info["NSAppleEventsUsageDescription"] as? String
            == "Pitboard opens Claude Code in Terminal with the Claude Desktop account you choose."
    )
    let entitlements = try plist("App/Pitboard.entitlements")
    #expect(entitlements["com.apple.security.automation.apple-events"] as? Bool == true)
    #expect(entitlements.count == 1)
    let project = try String(contentsOf: macos.appending(path: "project.yml"), encoding: .utf8)
    #expect(project.contains("CODE_SIGN_ENTITLEMENTS: App/Pitboard.entitlements"))
    let build = try String(
        contentsOf: macos.appending(path: "scripts/build-app.sh"), encoding: .utf8)
    #expect(build.contains(#"--entitlements apps/macos/App/Pitboard.entitlements "$app""#))
}

/// Counts how many runs are inside the runner at once.
private final class Overlap: @unchecked Sendable {
    private let lock = NSLock()
    private var running = 0
    private(set) var most = 0

    func run() {
        lock.lock()
        running += 1
        most = max(most, running)
        lock.unlock()
        Thread.sleep(forTimeInterval: 0.05)
        lock.lock()
        running -= 1
        lock.unlock()
    }
}

/// `NSAppleScript` off the main thread is run one script at a time, so two requests that
/// overlap never run the runner together.
@Test func overlappingScriptRunsAreSerialized() async throws {
    let helper = try DesktopCodeHelper()
    defer { helper.remove() }
    let overlap = Overlap()
    let tool = helper.tool { _ in
        overlap.run()
        return nil
    }
    async let first = tool.openDesktopCode(label: "first")
    async let second = tool.openDesktopCode(label: "second")
    _ = await (first, second)
    #expect(overlap.most == 1)
}

/// Asking to open Code first explains what happens; Terminal waits for the person's action.
@MainActor
@Test func desktopCodeShowsPreparationBeforeOpeningTerminal() async throws {
    let helper = try DesktopCodeHelper()
    defer { helper.remove() }
    let scripts = Scripts()
    let fixture = Fixture.claudeDesktop.dependencies(defaults: TestDefaults())
    let model = AppModel(
        testing: fixture.core, commandLineTool: helper.tool(scripts.run),
        appControl: fixture.appControl)
    await model.refresh()

    await model.openDesktopCode(label: "work")

    #expect(scripts.ran.isEmpty, "Terminal waits until the person is ready")
    #expect(model.sheet?.id == "desktopCode/work")
    #expect(model.requestedPane == .accounts)
}

/// Dismissing preparation or asking without it cannot dispatch the Terminal command.
@MainActor
@Test func desktopCodeCancelledPreparationDoesNotLaunch() async throws {
    let helper = try DesktopCodeHelper()
    defer { helper.remove() }
    let scripts = Scripts()
    let fixture = Fixture.claudeDesktop.dependencies(defaults: TestDefaults())
    let model = AppModel(
        testing: fixture.core, commandLineTool: helper.tool(scripts.run),
        appControl: fixture.appControl)
    await model.openDesktopCode(label: "work")
    model.sheet = nil

    #expect(await model.desktopCodeLaunchAsked(label: "work") != nil)
    #expect(scripts.ran.isEmpty)
}

/// Confirmation dispatches only the selected account; the sheet stays to explain the handoff.
@MainActor
@Test func desktopCodeConfirmationLaunchesThePreparedAccount() async throws {
    let helper = try DesktopCodeHelper()
    defer { helper.remove() }
    let scripts = Scripts()
    let fixture = Fixture.claudeDesktop.dependencies(defaults: TestDefaults())
    let model = AppModel(
        testing: fixture.core, commandLineTool: helper.tool(scripts.run),
        appControl: fixture.appControl)
    await model.openDesktopCode(label: "personal")
    #expect(await model.desktopCodeLaunchAsked(label: "work") != nil)
    #expect(scripts.ran.isEmpty)

    #expect(await model.desktopCodeLaunchAsked(label: "personal") == nil)
    #expect(
        scripts.ran == [
            CommandLineTool.script(openingDesktopCode: helper.path, label: "personal")
        ])
    #expect(model.sheet == .desktopCode(label: "personal"))
}

/// A macOS refusal or cancellation returns to preparation rather than reporting a handoff.
@MainActor
@Test(arguments: [false, true])
func desktopCodeDispatchFailureKeepsPreparation(cancelled: Bool) async throws {
    let helper = try DesktopCodeHelper()
    defer { helper.remove() }
    let fixture = Fixture.claudeDesktop.dependencies(defaults: TestDefaults())
    let model = AppModel(
        testing: fixture.core,
        commandLineTool: helper.tool { _ in
            [
                NSAppleScript.errorNumber: cancelled ? -128 : -1743,
                NSAppleScript.errorMessage: "Terminal refused the command.",
            ]
        }, appControl: fixture.appControl)
    await model.openDesktopCode(label: "work")

    let failure = try #require(await model.desktopCodeLaunchAsked(label: "work"))
    #expect(failure.title == (cancelled ? "Terminal wasn’t opened" : "Couldn’t open Terminal"))
    if cancelled {
        #expect(failure.message.contains("Choose Open Terminal"))
    } else {
        #expect(failure.message == "Terminal refused the command.")
    }
    #expect(model.sheet == .desktopCode(label: "work"))
    #expect(model.presentedFailure == nil)
}
