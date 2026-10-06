import AppKit
import Foundation
import PitboardKit
import PitboardLinkTarget

/// Everything the app reaches outside itself through: the core, the defaults it keeps its
/// own preferences in, the login item, other apps, the command line link, Notification
/// Center, and the sites and WebKit stores of the account windows.
///
/// Gathered in one value so a launch decides once which world the app runs in. A UI test
/// launches the debug build into a fixture, where every one of these is a stand-in and
/// nothing reaches the keychain, the network, launchd, the login items or an administrator
/// prompt of whoever runs the tests.
@MainActor
public struct Dependencies {
    let core: any Core
    let defaults: UserDefaults
    let loginItem: any LoginItem
    /// Other apps Pitboard may quit and open again around a switch.
    let appControl: any AppControl
    let commandLineTool: CommandLineTool
    /// Whether notifications are posted. Off in a fixture, where asking for permission
    /// would put a system prompt in front of the test.
    let notifies: Bool
    /// Whether the app reads on its own: the periodic read, the wake notice, the poll that
    /// notices a change made elsewhere, and the one repair of an older app's schedule.
    let watching: Bool
    /// The account windows' world: the sites, WebKit's stores and the Downloads folder, or a
    /// fixture's stand-ins.
    let web: WebEnvironment
    /// The scheme of the Pitboard links this build answers, which its Share extension hands
    /// links over in: `pitboard`, or `pitboard-debug` in a debug build.
    let linkScheme: String

    /// This machine, as the person running the app has it.
    public static func live() -> Dependencies {
        Dependencies(
            core: PitboardService.forThisApp(environment: environmentNamingClaudeApp()),
            defaults: .standard,
            loginItem: MainAppLoginItem(),
            appControl: WorkspaceAppControl(),
            commandLineTool: CommandLineTool(),
            notifies: true,
            watching: true,
            web: .live(defaults: .standard),
            linkScheme: LinkTarget.scheme(in: .main) ?? "pitboard")
    }

    /// The environment the core is started with, with the Claude app macOS knows of in it
    /// where nobody has named one. The core looks in /Applications otherwise, and Claude
    /// installed in ~/Applications or anywhere else would read there as not installed, its
    /// accounts left out of the app though Pitboard can quit and open it.
    private static func environmentNamingClaudeApp(
        _ environment: [String: String] = ProcessInfo.processInfo.environment,
        installed: (String) -> URL? = {
            NSWorkspace.shared.urlForApplication(withBundleIdentifier: $0)
        }
    ) -> [String: String] {
        var named = environment
        if named["PITBOARD_CLAUDE_DESKTOP_APP"]?.isEmpty ?? true,
            let app = installed(AppModel.claudeApp.bundleID)
        {
            named["PITBOARD_CLAUDE_DESKTOP_APP"] = app.path
        }
        return named
    }

    /// The world this launch runs in: `live()`, unless this is a debug build started with
    /// `PITBOARD_FIXTURE` naming one of the fixtures UI tests use.
    public static func forLaunch(
        environment: [String: String] = ProcessInfo.processInfo.environment
    ) -> Dependencies {
        #if DEBUG
            if let name = environment[Fixture.variable] {
                guard let fixture = Fixture(rawValue: name) else {
                    Launch.fail("no fixture is called \(name)")
                }
                return fixture.dependencies()
            }
        #endif
        return live()
    }
}
