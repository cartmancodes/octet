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
/// The module refuses anything larger; a store past this is not read at all.
private let entriesByteLimit = 64 * 1024
/// How much of a stale activity's content shows: dimmed, so it reads as old at a glance.
private let staleOpacity = 0.45

/// Where the particulars and the buttons are drawn. iOS caps the expanded Island at 160 points, so
/// there the command takes one line and Deny and Allow are 36 points tall; the Lock Screen has room
/// for two lines and 44-point buttons.
private enum Place {
  case lockScreen, island

  var commandLines: Int { self == .island ? 1 : 2 }
  var buttonHeight: CGFloat { self == .island ? 36 : 44 }
}

/// One session's waiting approval, as the app wrote it.
private struct Entry {
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

/// A host as the app keys the App Group by it: lowercased, without a trailing dot.
private func hostKey(_ host: String) -> String {
  (host.hasSuffix(".") ? String(host.dropLast()) : host).lowercased()
}

private func clip(_ text: String, _ limit: Int) -> String? {
  let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
  return trimmed.isEmpty ? nil : String(trimmed.prefix(limit))
}

/// What one content state shows.
private struct Shown {
  let state: OctetActivity.ContentState
  let entry: Entry?
  /// ActivityKit's word that the activity is past its stale-date (the hub and the app set one 15
  /// minutes on): nobody has updated it since, so what it shows may be old.
  let stale: Bool

  init(_ state: OctetActivity.ContentState, stale: Bool) {
    self.state = state
    self.stale = stale
    self.entry = Shown.entry(forKey: hostKey(state.mac) + "/" + state.session)
  }

  /// Reads only this session's entry, so another malformed entry hides nothing here.
  static func entry(forKey key: String) -> Entry? {
    guard let json = UserDefaults(suiteName: appGroup)?.string(forKey: entriesKey),
      json.utf8.count <= entriesByteLimit,
      let entries = try? JSONSerialization.jsonObject(with: Data(json.utf8)) as? [String: Any],
      let entry = entries[key] as? [String: Any],
      let name = entry["name"] as? String,
      let command = entry["command"] as? String
    else { return nil }
    return Entry(name: name, command: command, approval: entry["approval"] as? String)
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
  let place: Place

  var body: some View {
    // Both are the user's work: iOS may redact them on a locked phone, as the user sets it.
    if let name = shown.name {
      Text(name)
        .font(.subheadline)
        .lineLimit(1)
        .privacySensitive()
    }
    if let command = shown.command {
      Text(command)
        .font(.caption.monospaced())
        .foregroundStyle(.secondary)
        .lineLimit(place.commandLines)
        .privacySensitive()
    }
  }
}

/// "Stale", muted, under a stale activity's dimmed content; nothing otherwise.
private struct StaleLabel: View {
  let shown: Shown

  var body: some View {
    if shown.stale {
      Text("Stale")
        .font(.caption2.weight(.semibold))
        .foregroundStyle(.secondary)
        .lineLimit(1)
        .accessibilityLabel("Stale: this may be out of date")
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
      Particulars(shown: shown, place: .lockScreen)
    }
  }
}

private struct Actions: View {
  let shown: Shown
  let place: Place

  var body: some View {
    HStack(spacing: 8) {
      if let deny = shown.deny {
        Button(intent: deny) {
          Text("Deny")
            .frame(maxWidth: .infinity, minHeight: place.buttonHeight)
            .contentShape(Rectangle())
        }
        .tint(.red)
        .accessibilityLabel("Deny this approval")
      }
      Link(destination: shown.allowURL) {
        Text(shown.deny == nil ? "Open Octet" : "Allow")
          .font(.body.weight(.semibold))
          .frame(maxWidth: .infinity, minHeight: place.buttonHeight)
          .background(Capsule().fill(Color.green.opacity(0.35)))
          .contentShape(Rectangle())
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
      .opacity(shown.stale ? staleOpacity : 1)
      if shown.isApproval {
        Actions(shown: shown, place: .lockScreen)
          .opacity(shown.stale ? staleOpacity : 1)
      }
      StaleLabel(shown: shown)
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
      LockScreen(shown: Shown(context.state, stale: context.isStale))
        .widgetURL(URL(string: "octet://"))
    } dynamicIsland: { context in
      let shown = Shown(context.state, stale: context.isStale)
      return DynamicIsland {
        DynamicIslandExpandedRegion(.leading) {
          MascotImage(pose: shown.pose, height: 36)
            .opacity(shown.stale ? staleOpacity : 1)
        }
        DynamicIslandExpandedRegion(.trailing) {
          Elapsed(shown: shown)
            .font(.headline)
            .frame(maxWidth: 72, alignment: .trailing)
            .opacity(shown.stale ? staleOpacity : 1)
        }
        DynamicIslandExpandedRegion(.center) {
          VStack(spacing: 2) {
            Text(shown.title)
              .font(.headline)
              .lineLimit(1)
              .opacity(shown.stale ? staleOpacity : 1)
            StaleLabel(shown: shown)
          }
        }
        DynamicIslandExpandedRegion(.bottom) {
          VStack(alignment: .leading, spacing: 6) {
            Particulars(shown: shown, place: .island)
            if shown.isApproval {
              Actions(shown: shown, place: .island)
            }
          }
          .opacity(shown.stale ? staleOpacity : 1)
        }
      } compactLeading: {
        MascotImage(pose: shown.pose, height: 18)
          .opacity(shown.stale ? staleOpacity : 1)
      } compactTrailing: {
        CompactTrailing(shown: shown)
          .opacity(shown.stale ? staleOpacity : 1)
      } minimal: {
        MascotImage(pose: shown.pose, height: 16)
          .opacity(shown.stale ? staleOpacity : 1)
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
