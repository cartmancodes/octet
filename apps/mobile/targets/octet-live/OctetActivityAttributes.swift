import ActivityKit
import Foundation

// The Live Activity's attributes, with no fields of their own. Push-to-start matches on the type's
// name, so it is exactly the hub's "attributes-type": "OctetActivity". This file is copied verbatim
// into targets/octet-live (a test keeps the copies equal), so the app and the widget decode one shape.
struct OctetActivity: ActivityAttributes {
  // Exactly the hub's "content-state". `session` is the hub's raw session id or "", and `mac` its
  // published host or "". `kind` stays a String so an unknown kind still decodes.
  struct ContentState: Codable, Hashable {
    let kind: String
    let session: String
    let mac: String
    let since: Double
    let approvals: Int
  }
}
