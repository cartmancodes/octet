import ActivityKit
import ExpoModulesCore
import Foundation

// The App Group the widget shares, and the keys both sides use. Tests hold these strings equal in
// this file, targets/octet-live/OctetLiveWidget.swift and targets/octet-live/_shared/DenyIntent.swift.
private let appGroup = "group.dev.octet.mobile"
private let entriesKey = "entries"
private let deniesKey = "denies"
private let denyNotification = "dev.octet.mobile.deny"

/// The content state as JavaScript passes it: the same five fields as the hub's payload.
struct ActivityStateRecord: Record {
  @Field var kind: String = "running"
  @Field var session: String = ""
  @Field var mac: String = ""
  @Field var since: Double = 0
  @Field var approvals: Int = 0

  var content: ActivityContent<OctetActivity.ContentState> {
    let state = OctetActivity.ContentState(
      kind: kind, session: session, mac: mac, since: since, approvals: approvals)
    return ActivityContent(state: state, staleDate: nil)
  }
}

private func hex(_ data: Data) -> String {
  data.map { String(format: "%02x", $0) }.joined()
}

/// Starts, updates and ends the Live Activity, reports its ActivityKit tokens, and passes on the
/// widget's Deny taps. Each event is watched only while JavaScript listens for it.
public final class OctetLiveModule: Module {
  private let lock = NSLock()
  private var tasks: [String: Task<Void, Never>] = [:]
  private var hearingDenies = false

  public func definition() -> ModuleDefinition {
    Name("OctetLive")

    Events("pushToken", "pushToStartToken", "deny")

    Function("isSupported") { () -> Bool in
      ActivityAuthorizationInfo().areActivitiesEnabled
    }

    // Starts the activity, or updates the one that runs already (the hub may have started it).
    AsyncFunction("start") { (state: ActivityStateRecord) async throws in
      if let running = Activity<OctetActivity>.activities.first {
        await running.update(state.content)
      } else {
        _ = try Activity<OctetActivity>.request(
          attributes: OctetActivity(), content: state.content, pushType: .token)
      }
    }

    AsyncFunction("update") { (state: ActivityStateRecord) async in
      for activity in Activity<OctetActivity>.activities {
        await activity.update(state.content)
      }
    }

    AsyncFunction("end") { () async in
      for activity in Activity<OctetActivity>.activities {
        await activity.end(nil, dismissalPolicy: .immediate)
      }
    }

    // The app's cleaned and bounded {"<host>/<raw>": {name, command, approval}} as JSON.
    AsyncFunction("setEntries") { (json: String) in
      let defaults = UserDefaults(suiteName: appGroup)
      defaults?.set(json, forKey: entriesKey)
    }

    OnStartObserving("pushToStartToken") {
      self.track("start") {
        Task { [weak self] in
          for await data in Activity<OctetActivity>.pushToStartTokenUpdates {
            self?.sendEvent("pushToStartToken", ["token": hex(data)])
          }
        }
      }
    }

    OnStopObserving("pushToStartToken") {
      self.cancel { $0 == "start" }
    }

    OnStartObserving("pushToken") {
      self.track("activities") {
        Task { [weak self] in
          for await activity in Activity<OctetActivity>.activityUpdates {
            self?.watch(activity)
          }
        }
      }
      for activity in Activity<OctetActivity>.activities {
        self.watch(activity)
      }
    }

    OnStopObserving("pushToken") {
      self.cancel { $0 != "start" }
    }

    OnStartObserving("deny") {
      self.hearDenies(true)
    }

    OnStopObserving("deny") {
      self.hearDenies(false)
    }

    OnDestroy {
      self.cancel { _ in true }
      self.hearDenies(false)
    }
  }

  /// Reports one activity's push token as it changes, and null once the activity is over.
  private func watch(_ activity: Activity<OctetActivity>) {
    track("token:" + activity.id) {
      Task { [weak self] in
        for await data in activity.pushTokenUpdates {
          self?.sendEvent("pushToken", ["token": hex(data)])
        }
      }
    }
    track("state:" + activity.id) {
      Task { [weak self] in
        for await state in activity.activityStateUpdates where state == .ended || state == .dismissed {
          self?.sendEvent("pushToken", ["token": nil])
          return
        }
      }
    }
  }

  private func track(_ key: String, _ make: () -> Task<Void, Never>) {
    lock.lock()
    defer { lock.unlock() }
    if tasks[key] == nil {
      tasks[key] = make()
    }
  }

  private func cancel(_ matching: (String) -> Bool) {
    lock.lock()
    defer { lock.unlock() }
    for (key, task) in tasks where matching(key) {
      task.cancel()
      tasks[key] = nil
    }
  }

  /// While JavaScript listens, a Darwin notification from DenyIntent drains the App Group's queue;
  /// starting to listen drains it too, so a Deny tapped before the app was up is not lost.
  private func hearDenies(_ on: Bool) {
    let center = CFNotificationCenterGetDarwinNotifyCenter()
    let observer = Unmanaged.passUnretained(self).toOpaque()
    lock.lock()
    let was = hearingDenies
    hearingDenies = on
    lock.unlock()
    if on && !was {
      CFNotificationCenterAddObserver(
        center, observer,
        { _, observer, _, _, _ in
          guard let observer else { return }
          Unmanaged<OctetLiveModule>.fromOpaque(observer).takeUnretainedValue().drainDenies()
        },
        denyNotification as CFString, nil, .deliverImmediately)
      drainDenies()
    } else if !on && was {
      CFNotificationCenterRemoveObserver(
        center, observer, CFNotificationName(denyNotification as CFString), nil)
    }
  }

  private func drainDenies() {
    guard let defaults = UserDefaults(suiteName: appGroup) else { return }
    let pending = defaults.array(forKey: deniesKey) as? [[String: String]] ?? []
    defaults.removeObject(forKey: deniesKey)
    for deny in pending {
      sendEvent(
        "deny",
        [
          "mac": deny["mac"] ?? "",
          "session": deny["session"] ?? "",
          "approval": deny["approval"] ?? "",
        ])
    }
  }
}
