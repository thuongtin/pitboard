import Foundation
import Testing

@testable import PitboardKit

/// A scratch home whose Claude Code and Codex directories are its own, so the credential
/// slot read is hashed from it and never the machine's real login, and the Codex login read
/// is a file that is not there. The test that makes one removes it, so no run leaves homes
/// behind in the temporary directory.
private struct ScratchHome {
    let root = FileManager.default.temporaryDirectory
        .appendingPathComponent("pitboardkit-\(UUID().uuidString)")

    init() throws {
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
    }

    /// A Claude app of its own, which is not there unless `desktop` makes one, so nothing
    /// looks in /Applications.
    var desktopApp: String { root.appendingPathComponent("Claude.app").path }

    func settings(codex: String? = nil, schedules: String? = nil) -> Settings {
        Settings(
            home: root.path,
            pitboardHome: root.appendingPathComponent("pitboard").path,
            claudeConfigDir: root.appendingPathComponent("claude").path,
            secureStorageDir: nil,
            user: NSUserName(),
            claudeProgram: nil,
            codexHome: root.appendingPathComponent("codex").path,
            codexProgram: codex,
            scheduleProgram: schedules,
            desktopDir: root.appendingPathComponent("claude-desktop").path,
            desktopApp: desktopApp
        )
    }

    func remove() { try? FileManager.default.removeItem(at: root) }
}

@Test func statusOfAnEmptyHomeHasNoAccounts() async throws {
    let home = try ScratchHome()
    defer { home.remove() }
    let status = try await PitboardService(settings: home.settings()).status(fresh: false)
    #expect(status.accounts.isEmpty)
    #expect(status.warnings.isEmpty)
}

@Test func aFailedChangeCarriesItsStableCode() async throws {
    let home = try ScratchHome()
    defer { home.remove() }
    let service = PitboardService(settings: home.settings())
    do {
        _ = try await service.rename("nobody", to: "somebody")
        Issue.record("renaming an account that does not exist must fail")
    } catch let PitboardError.Failed(code, cause, message, _) {
        #expect(code == "account_unknown")
        #expect(message.contains("nobody"))
        #expect(cause == nil, "nothing was asked of Anthropic, so nothing went wrong there")
    }
}

/// A tool is offered only where its program was found, and a program named outright
/// counts as found: that is what `PITBOARD_CLAUDE` and `PITBOARD_CODEX` are for.
@Test func onlyToolsWithAProgramAreInstalled() async throws {
    let (bare, withCodex) = (try ScratchHome(), try ScratchHome())
    defer {
        bare.remove()
        withCodex.remove()
    }
    let neither = PitboardService(settings: bare.settings())
    #expect(neither.tools().map(\.code) == ["claude", "codex", "desktop"])
    #expect(await neither.installed().isEmpty)
    let codex = PitboardService(settings: withCodex.settings(codex: "/nowhere/codex"))
    #expect(await codex.installed().map(\.code) == ["codex"])
}

/// Claude Desktop is installed where its app has its program, which is looked for in the
/// app named outright and never asks a login shell.
@Test func claudeDesktopIsInstalledWhereItsAppIs() async throws {
    let home = try ScratchHome()
    defer { home.remove() }
    let program = URL(fileURLWithPath: home.desktopApp).appendingPathComponent(
        "Contents/MacOS/Claude")
    try FileManager.default.createDirectory(
        at: program.deletingLastPathComponent(), withIntermediateDirectories: true)
    FileManager.default.createFile(
        atPath: program.path, contents: Data("#!/bin/sh\n".utf8),
        attributes: [.posixPermissions: 0o755])
    let service = PitboardService(settings: home.settings())
    #expect(await service.installed().map(\.code) == ["desktop"])
}

/// The Claude app and its data are read from the variables the command line reads, so the
/// two look at the same Claude.
@Test func theDesktopVariablesAreRead() {
    let settings = Settings.forCurrentUser(
        environment: [
            "HOME": "/scratch", "PITBOARD_CLAUDE_DESKTOP_DIR": "/scratch/support",
            "PITBOARD_CLAUDE_DESKTOP_APP": "/scratch/Claude.app",
        ],
        loginPath: nil, bundle: nil, isExecutable: { _ in false })
    #expect(settings.desktopDir == "/scratch/support")
    #expect(settings.desktopApp == "/scratch/Claude.app")
}

/// Counts how often the settings were asked for, and whether any ask was on the main
/// thread.
private final class Asks: @unchecked Sendable {
    // Unchecked because the counts are written from the service's queues; every read and
    // write holds `lock`.
    private let lock = NSLock()
    private var asked = 0
    private var onMain = false

    func note() {
        let main = Thread.isMainThread
        lock.withLock {
            asked += 1
            onMain = onMain || main
        }
    }

    var count: Int { lock.withLock { asked } }
    var anyOnMain: Bool { lock.withLock { onMain } }
}

/// Where the tools are can take asking the person's login shell, so the settings are asked
/// for when something first needs the core, off the main thread, and once however many
/// calls arrive at the same time.
@Test func theSettingsAreAskedForOnceOffTheMainThread() async throws {
    let home = try ScratchHome()
    defer { home.remove() }
    let settings = home.settings(codex: "/nowhere/codex")
    let asks = Asks()
    let service = PitboardService {
        asks.note()
        return settings
    }
    #expect(service.tools().count == 3, "listing the tools asks nothing")
    #expect(asks.count == 0)
    async let installed = service.installed()
    async let changed = service.changedAt()
    async let diagnosis = service.doctor()
    let (found, _, _) = await (installed, changed, diagnosis)
    #expect(found.map(\.code) == ["codex"])
    #expect(asks.count == 1)
    #expect(!asks.anyOnMain)
}

/// A login shell too slow to answer, which startup files are while the machine is busy
/// logging in, is asked once more when something next asks what is installed, a while later,
/// and what it answers then is what is used. Once more and no more: a shell that is always
/// that slow would otherwise cost its patience on every ask.
@Test func aLoginShellTooSlowToAnswerIsAskedOnceMoreLater() async throws {
    let (late, found) = (try ScratchHome(), try ScratchHome())
    defer {
        late.remove()
        found.remove()
    }
    let (slow, answered) = (late.settings(), found.settings(codex: "/nowhere/codex"))
    let asks = Asks()
    let service = PitboardService(
        asking: {
            asks.note()
            return asks.count == 1 ? (slow, true) : (answered, false)
        }, askAgainAfter: 0.2)

    #expect(await service.installed().isEmpty, "what the first, late, ask found")
    #expect(await service.installed().isEmpty, "and nothing more until the while is up")
    #expect(asks.count == 1)
    try await Task.sleep(for: .milliseconds(300))
    #expect(await service.installed().map(\.code) == ["codex"])
    #expect(asks.count == 2)
    #expect(await service.installed().map(\.code) == ["codex"])
    #expect(asks.count == 2, "asked once more, and no more")
}

/// Not before the while is up, and never when the shell answered or could not be asked.
@Test func aLoginShellIsNotAskedAgainSoonerOrForNothing() async throws {
    let home = try ScratchHome()
    defer { home.remove() }
    let settings = home.settings()
    let soon = Asks()
    let early = PitboardService(
        asking: {
            soon.note()
            return (settings, true)
        }, askAgainAfter: 3600)
    _ = await early.installed()
    _ = await early.installed()
    #expect(soon.count == 1)

    let answered = Asks()
    let service = PitboardService(
        asking: {
            answered.note()
            return (settings, false)
        }, askAgainAfter: 0)
    _ = await service.installed()
    _ = await service.installed()
    #expect(answered.count == 1)
}

@Test func doctorReportsEveryCheck() async throws {
    let home = try ScratchHome()
    defer { home.remove() }
    let diagnosis = await PitboardService(settings: home.settings()).doctor()
    #expect(!diagnosis.checks.isEmpty)
    #expect(diagnosis.checks.contains { $0.code == "state" })
}

/// The app asks for a repair every time it starts, so where there is nothing to repair it
/// answers that and changes nothing. A scratch home has no schedule of its own.
@Test func aHomeWithNoScheduleHasNothingToRepair() async throws {
    let bundled = FileManager.default.temporaryDirectory
        .appendingPathComponent("pitboardkit-helper-\(UUID().uuidString)")
    try Data("#!/bin/sh\n".utf8).write(to: bundled)
    defer { try? FileManager.default.removeItem(at: bundled) }
    for program in [nil, bundled.path] {
        let home = try ScratchHome()
        defer { home.remove() }
        let service = PitboardService(settings: home.settings(schedules: program))
        #expect(try await service.scheduleRepair() == false)
    }
}
