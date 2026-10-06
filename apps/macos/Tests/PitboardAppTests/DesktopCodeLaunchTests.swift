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
        environment: ["PITBOARD_HOME": "scratch/state", "PITBOARD_CLAUDE_DESKTOP_DIR": "scratch/data"],
        execute: { _ in nil })
    let here = FileManager.default.currentDirectoryPath
    #expect(tool.codeEnvironment["PITBOARD_HOME"] == "\(here)/scratch/state")
    #expect(tool.codeEnvironment["PITBOARD_CLAUDE_DESKTOP_DIR"] == "\(here)/scratch/data")
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
