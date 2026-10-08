import PitboardKit
import SwiftUI

/// Keeps the preparation visible until Terminal takes over, without claiming Code is ready.
struct DesktopCodeSheet: View {
    let model: AppModel
    let label: String
    @State private var failure: ActionFailure?
    @State private var working = false
    @State private var handedOff = false
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        SheetLayout(
            title: handedOff ? "Continue in Terminal" : "Open Claude Code",
            message: handedOff
                ? "The command was sent to Terminal. Complete the next steps there."
                : "Review the steps before opening a Code session with this Desktop account."
        ) {
            Section("Desktop account") {
                LabeledContent("Name", value: label)
                if let account = model.status?.accounts.first(where: {
                    $0.provider == desktopProvider && $0.label == label
                }), !account.email.isEmpty {
                    LabeledContent("Email", value: account.email)
                }
            }
            Section(handedOff ? "Next, in Terminal" : "What happens next") {
                step(
                    1, title: handedOff ? "Command sent to Terminal" : "Open Terminal",
                    message: "Pitboard starts a command for \(label).", complete: handedOff)
                step(
                    2, title: "Approve macOS requests if asked",
                    message: "Allow Pitboard to control Terminal. If Claude Safe Storage asks "
                        + "for a password, use your Mac login password.")
                step(
                    3, title: "Wait for account verification",
                    message: "The command checks the selected account before starting Code. "
                        + "Follow any instructions in Terminal, then begin your task.")
            }
            if let failure {
                SheetFailure(failure: failure)
            }
        } buttons: {
            if working {
                ProgressView().controlSize(.small)
                Text("Opening Terminal…").foregroundStyle(.secondary)
            }
            if handedOff {
                Button("Done") { dismiss() }
                    .keyboardShortcut(.defaultAction)
            } else {
                Button("Cancel", role: .cancel) { dismiss() }
                    .keyboardShortcut(.cancelAction)
                    .disabled(working)
                Button("Open Terminal", action: launch)
                    .keyboardShortcut(.defaultAction)
                    .disabled(working)
            }
        }
        .interactiveDismissDisabled(working)
    }

    private func step(_ number: Int, title: String, message: String, complete: Bool = false)
        -> some View
    {
        HStack(alignment: .top, spacing: 12) {
            Image(systemName: complete ? "checkmark.circle.fill" : "\(number).circle")
                .font(.title3)
                .foregroundStyle(complete ? Color.accentColor : Color.secondary)
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: Design.lineSpacing) {
                Text(title).fontWeight(.medium)
                Text(message).explanatory()
            }
        }
        .accessibilityElement(children: .combine)
        .accessibilityLabel("Step \(number). \(title). \(message)")
    }

    private func launch() {
        guard !working, !handedOff else { return }
        working = true
        failure = nil
        Task {
            failure = await model.desktopCodeLaunchAsked(label: label)
            handedOff = failure == nil
            working = false
        }
    }
}
