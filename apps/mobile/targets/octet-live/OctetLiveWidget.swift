import ActivityKit
import AppIntents
import SwiftUI
import WidgetKit

// The Live Activity in the Dynamic Island and on the Lock Screen. Its text comes only from the
// content state (kind, approvals, since) and from the App Group, where the app writes
// {"<host>/<raw session>": {name, command, approval}}; the push itself names nothing. The keys match
// modules/octet-live/ios/OctetLiveModule.swift.
private let appGroup = "group.dev.octet.mobile"
private let entriesKey = "entries"

/// One session's waiting approval, as the app wrote it.
private struct Entry: Decodable {
  let name: String
  let command: String
  let approval: String?
}

/// The unreserved URL characters; everything else in a link part is percent-encoded.
private let unreserved = CharacterSet(
  charactersIn: "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~")

private func linkPart(_ text: String) -> String {
  text.addingPercentEncoding(withAllowedCharacters: unreserved) ?? ""
}

private func clip(_ text: String, _ limit: Int) -> String? {
  let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
  return trimmed.isEmpty ? nil : String(trimmed.prefix(limit))
}

/// What one content state shows.
private struct Shown {
  let state: OctetActivity.ContentState
  let entry: Entry?

  init(_ state: OctetActivity.ContentState) {
    self.state = state
    self.entry = Shown.entry(forKey: state.mac + "/" + state.session)
  }

  static func entry(forKey key: String) -> Entry? {
    guard let json = UserDefaults(suiteName: appGroup)?.string(forKey: entriesKey),
      let data = json.data(using: .utf8),
      let entries = try? JSONDecoder().decode([String: Entry].self, from: data)
    else { return nil }
    return entries[key]
  }

  var isApproval: Bool { state.kind == "approval" }
  var isDone: Bool { state.kind == "done" }
  var since: Date { Date(timeIntervalSince1970: state.since) }

  /// One pose per kind: running codes, an approval asks, done succeeds.
  var pose: String {
    if isApproval { return "mascot-approval" }
    if isDone { return "mascot-success" }
    return "mascot-coding"
  }

  var title: String {
    if isApproval {
      if entry != nil || state.approvals == 0 { return "Approval waiting" }
      return state.approvals == 1 ? "1 approval waiting" : "\(state.approvals) approvals waiting"
    }
    return isDone ? "Done" : "Session running"
  }

  var name: String? { entry.flatMap { clip($0.name, 80) } }
  var command: String? { isApproval ? entry.flatMap { clip($0.command, 200) } : nil }

  /// The approval's id, when the app shared one for this session.
  var approval: String? {
    guard isApproval, let id = entry?.approval, !id.isEmpty else { return nil }
    return id
  }

  /// Deny answers in place; with no known approval there is no Deny.
  var deny: DenyIntent? {
    approval.map { DenyIntent(mac: state.mac, session: state.session, approval: $0) }
  }

  /// Allow opens the app at that approval, which asks for Face ID and the arm; with no known
  /// approval it opens the Inbox.
  var allowURL: URL {
    guard let approval else { return inboxURL }
    let link =
      "octet://approve/"
      + [state.mac, state.session, approval].map(linkPart).joined(separator: "/")
    return URL(string: link) ?? inboxURL
  }

  var inboxURL: URL { URL(string: "octet://")! }
}

private struct MascotImage: View {
  let pose: String
  let height: CGFloat

  var body: some View {
    // The poses are 59 × 43 pixel art: scale them without smoothing.
    Image(pose)
      .interpolation(.none)
      .resizable()
      .aspectRatio(59.0 / 43.0, contentMode: .fit)
      .frame(height: height)
      .accessibilityLabel("Octet")
  }
}

private struct Elapsed: View {
  let shown: Shown

  var body: some View {
    if shown.isDone {
      Image(systemName: "checkmark")
    } else {
      Text(shown.since, style: .timer)
        .monospacedDigit()
        .multilineTextAlignment(.trailing)
    }
  }
}

/// The session's name and the command waiting, when the app shared them.
private struct Particulars: View {
  let shown: Shown

  var body: some View {
    if let name = shown.name {
      Text(name)
        .font(.subheadline)
        .lineLimit(1)
    }
    if let command = shown.command {
      Text(command)
        .font(.caption.monospaced())
        .foregroundStyle(.secondary)
        .lineLimit(2)
    }
  }
}

private struct Details: View {
  let shown: Shown

  var body: some View {
    VStack(alignment: .leading, spacing: 2) {
      Text(shown.title)
        .font(.headline)
        .lineLimit(1)
      Particulars(shown: shown)
    }
  }
}

private struct Actions: View {
  let shown: Shown

  var body: some View {
    HStack(spacing: 8) {
      if let deny = shown.deny {
        Button(intent: deny) {
          Text("Deny")
            .frame(maxWidth: .infinity)
        }
        .tint(.red)
        .accessibilityLabel("Deny this approval")
      }
      Link(destination: shown.allowURL) {
        Text(shown.deny == nil ? "Open Octet" : "Allow")
          .font(.body.weight(.semibold))
          .frame(maxWidth: .infinity, minHeight: 32)
          .background(Capsule().fill(Color.green.opacity(0.35)))
      }
      .accessibilityLabel(shown.deny == nil ? "Open the Inbox" : "Allow, in Octet")
    }
  }
}

private struct LockScreen: View {
  let shown: Shown

  var body: some View {
    VStack(alignment: .leading, spacing: 10) {
      HStack(alignment: .center, spacing: 10) {
        MascotImage(pose: shown.pose, height: 36)
        Details(shown: shown)
        Spacer(minLength: 0)
        Elapsed(shown: shown)
          .font(.headline)
          .frame(maxWidth: 72, alignment: .trailing)
      }
      if shown.isApproval {
        Actions(shown: shown)
      }
    }
    .padding(14)
  }
}

private struct CompactTrailing: View {
  let shown: Shown

  var body: some View {
    if shown.isApproval {
      Text("\(max(shown.state.approvals, 1))")
        .font(.headline)
        .foregroundStyle(.orange)
    } else {
      Elapsed(shown: shown)
        .frame(maxWidth: 44)
    }
  }
}

struct OctetLiveWidget: Widget {
  var body: some WidgetConfiguration {
    ActivityConfiguration(for: OctetActivity.self) { context in
      LockScreen(shown: Shown(context.state))
        .widgetURL(URL(string: "octet://"))
    } dynamicIsland: { context in
      let shown = Shown(context.state)
      return DynamicIsland {
        DynamicIslandExpandedRegion(.leading) {
          MascotImage(pose: shown.pose, height: 36)
        }
        DynamicIslandExpandedRegion(.trailing) {
          Elapsed(shown: shown)
            .font(.headline)
            .frame(maxWidth: 72, alignment: .trailing)
        }
        DynamicIslandExpandedRegion(.center) {
          Text(shown.title)
            .font(.headline)
            .lineLimit(1)
        }
        DynamicIslandExpandedRegion(.bottom) {
          VStack(alignment: .leading, spacing: 6) {
            Particulars(shown: shown)
            if shown.isApproval {
              Actions(shown: shown)
            }
          }
        }
      } compactLeading: {
        MascotImage(pose: shown.pose, height: 18)
      } compactTrailing: {
        CompactTrailing(shown: shown)
      } minimal: {
        MascotImage(pose: shown.pose, height: 16)
      }
      .widgetURL(shown.inboxURL)
    }
  }
}

@main
struct OctetLiveBundle: WidgetBundle {
  var body: some Widget {
    OctetLiveWidget()
  }
}
