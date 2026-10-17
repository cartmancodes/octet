import AppIntents
import Foundation

// The widget's Deny button. It sits in _shared, so it is compiled into the app as well as the widget
// and the system runs it in the app's process. It answers nothing itself: it writes the request under
// its own App Group key, `deny.<uuid>`, and pings the app with a Darwin notification. The Octet Live
// module drains every `deny.*` key and hands JavaScript a `deny {mac, session, approval, id}` event,
// which resolves the Mac with fleet.keyFor and sends `answer allow:false`. iOS keeps the app running
// only until perform() returns, so it waits (at most 9 s) for the app's ack, `denied.<uuid>`, which
// the module writes once JavaScript says the send settled. The keys match
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
    // Expired (older than 10 minutes, which the app would drop) and malformed waiting denies go
    // first, so old taps never fill the 16 and block this one; so do acks no wait was left to read.
    let now = Date().timeIntervalSince1970
    for key in defaults.dictionaryRepresentation().keys where key.hasPrefix("deny.") {
      let at = defaults.dictionary(forKey: key)?["at"] as? Double
      if at.map({ abs(now - $0) > 10 * 60 }) ?? true {
        defaults.removeObject(forKey: key)
      }
    }
    for key in defaults.dictionaryRepresentation().keys where key.hasPrefix("denied.") {
      let at = defaults.object(forKey: key) as? Double
      if at.map({ abs(now - $0) > 10 * 60 }) ?? true {
        defaults.removeObject(forKey: key)
      }
    }
    // At most 16 wait for the app; past that, this tap is dropped (the app drains them whenever it
    // listens).
    let waiting = defaults.dictionaryRepresentation().keys.filter { $0.hasPrefix("deny.") }
    if waiting.count >= 16 { return .result() }
    // A key of its own: nothing else writes it, so no other Deny can overwrite or drop this one.
    let deny: [String: Any] = [
      "mac": mac, "session": session, "approval": approval, "at": Date().timeIntervalSince1970,
    ]
    let id = UUID().uuidString
    defaults.set(deny, forKey: "deny." + id)
    CFNotificationCenterPostNotification(
      CFNotificationCenterGetDarwinNotifyCenter(),
      CFNotificationName("dev.octet.mobile.deny" as CFString), nil, nil, true)
    // The app is suspended as soon as this returns, before its send could reach the Mac: wait for
    // its ack of this request, checked every 100 ms, for at most 9 s. A Deny the app doesn't ack in
    // time is still sent when it can (the module holds background time of its own for it).
    let ack = "denied." + id
    let deadline = Date().addingTimeInterval(9)
    while Date() < deadline {
      if defaults.object(forKey: ack) != nil {
        defaults.removeObject(forKey: ack)
        break
      }
      try? await Task.sleep(nanoseconds: 100_000_000)
    }
    return .result()
  }
}
