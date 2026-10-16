import AppIntents
import Foundation

// The widget's Deny button. It sits in _shared, so it is compiled into the app as well as the widget
// and the system runs it in the app's process. It answers nothing itself: it queues the request in the
// App Group and pings the app with a Darwin notification. The Octet Live module drains the queue and
// hands JavaScript a `deny {mac, session, approval}` event, which resolves the Mac with fleet.keyFor
// and sends `answer allow:false`. The keys match modules/octet-live/ios/OctetLiveModule.swift.
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
    var pending = defaults.array(forKey: "denies") as? [[String: String]] ?? []
    pending.append(["mac": mac, "session": session, "approval": approval])
    // A handful of taps at most wait for the app; keep only the newest few.
    defaults.set(Array(pending.suffix(8)), forKey: "denies")
    CFNotificationCenterPostNotification(
      CFNotificationCenterGetDarwinNotifyCenter(),
      CFNotificationName("dev.octet.mobile.deny" as CFString), nil, nil, true)
    return .result()
  }
}
