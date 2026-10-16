import AppIntents
import Foundation

// The widget's Deny button. It sits in _shared, so it is compiled into the app as well as the widget
// and the system runs it in the app's process. It answers nothing itself: it writes the request under
// its own App Group key, `deny.<uuid>`, and pings the app with a Darwin notification. The Octet Live
// module drains every `deny.*` key and hands JavaScript a `deny {mac, session, approval}` event, which
// resolves the Mac with fleet.keyFor and sends `answer allow:false`. The keys match
// modules/octet-live/ios/OctetLiveModule.swift. The app runs on iOS 16.4; the Island needs 17.2.
@available(iOS 17.2, *)
struct DenyIntent: LiveActivityIntent {
  static let title: LocalizedStringResource = "Deny"
  static let isDiscoverable = false

  /// The hub's published host, as the content state carried it ("" when it had none).
  @Parameter(title: "Mac") var mac: String
  /// The hub's raw session id.
  @Parameter(title: "Session") var session: String
  /// The approval's id, from the App Group entry.
  @Parameter(title: "Approval") var approval: String

  init() {}

  init(mac: String, session: String, approval: String) {
    self.mac = mac
    self.session = session
    self.approval = approval
  }

  func perform() async throws -> some IntentResult {
    guard !session.isEmpty, !approval.isEmpty,
      let defaults = UserDefaults(suiteName: "group.dev.octet.mobile")
    else { return .result() }
    // A key of its own: nothing else writes it, so no other Deny can overwrite or drop this one.
    defaults.set(
      ["mac": mac, "session": session, "approval": approval],
      forKey: "deny." + UUID().uuidString)
    CFNotificationCenterPostNotification(
      CFNotificationCenterGetDarwinNotifyCenter(),
      CFNotificationName("dev.octet.mobile.deny" as CFString), nil, nil, true)
    return .result()
  }
}
