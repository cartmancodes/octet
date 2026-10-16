import ActivityKit
import ExpoModulesCore
import Foundation

// The App Group the widget shares, and the keys both sides use. Tests hold these strings equal in
// this file, targets/octet-live/OctetLiveWidget.swift and targets/octet-live/_shared/DenyIntent.swift.
private let appGroup = "group.dev.octet.mobile"
private let entriesKey = "entries"
/// The entries JSON may be at most this many bytes, with at most this many sessions.
private let entriesByteLimit = 64 * 1024
private let entriesLimit = 64
/// Each Deny tapped in the widget waits under its own `deny.<uuid>` key.
private let denyPrefix = "deny."
private let denyNotification = "dev.octet.mobile.deny"
/// A Deny older than this is dropped, not sent: its approval has long moved on.
private let denyMaxAge: TimeInterval = 10 * 60

/// The content state as JavaScript passes it: the same five fields as the hub's payload.
struct ActivityStateRecord: Record {
  @Field var kind: String = "running"
  @Field var session: String = ""
  @Field var mac: String = ""
  @Field var since: Double = 0
  @Field var approvals: Int = 0
}

@available(iOS 17.2, *)
extension ActivityStateRecord {
  var content: ActivityContent<OctetActivity.ContentState> {
    let state = OctetActivity.ContentState(
      kind: kind, session: session, mac: mac, since: since, approvals: approvals)
    return ActivityContent(state: state, staleDate: nil)
  }
}

final class InvalidEntriesException: Exception, @unchecked Sendable {
  override var reason: String {
    "entries must be a JSON object of at most \(entriesLimit) objects of strings, within \(entriesByteLimit) bytes"
  }
}

/// The activities still on screen; an ended or dismissed one is never started or updated again.
@available(iOS 17.2, *)
private func liveActivities() -> [Activity<OctetActivity>] {
  Activity<OctetActivity>.activities.filter {
    $0.activityState == .active || $0.activityState == .stale
  }
}

private func hex(_ data: Data) -> String {
  data.map { String(format: "%02x", $0) }.joined()
}

/// Starts, updates and ends the Live Activity, reports its ActivityKit tokens, and passes on the
/// widget's Deny taps. Each event is watched only while JavaScript listens for it.
///
/// The app runs on iOS 16.4, but the activity needs 17.2 (push-to-start tokens). `OctetActivity` is
/// marked `@available(iOS 17.2, *)`, so every ActivityKit call here sits behind `#available`; below
/// 17.2 isSupported() is false and every function resolves without doing anything.
// @unchecked Sendable: its only mutable state, `tasks` and `hearingDenies`, is behind `lock`.
public final class OctetLiveModule: Module, @unchecked Sendable {
  /// Guards `tasks` and `hearingDenies`.
  private let lock = NSLock()
  /// Each watching task by key, with an id so a finished task forgets only itself.
  private var tasks: [String: (id: UUID, task: Task<Void, Never>)] = [:]
  private var hearingDenies = false
  /// Drains run one at a time, so a key is emitted once before it is removed.
  private let denyQueue = DispatchQueue(label: "dev.octet.mobile.deny")

  public func definition() -> ModuleDefinition {
    Name("OctetLive")

    Events("pushToken", "pushToStartToken", "deny")

    Function("isSupported") { () -> Bool in
      if #available(iOS 17.2, *) {
        return ActivityAuthorizationInfo().areActivitiesEnabled
      }
      return false
    }

    // Starts the activity, or updates the live one (the hub may have started it).
    AsyncFunction("start") { (state: ActivityStateRecord) async throws in
      guard #available(iOS 17.2, *) else { return }
      if let running = liveActivities().first {
        await running.update(state.content)
      } else {
        _ = try Activity<OctetActivity>.request(
          attributes: OctetActivity(), content: state.content, pushType: .token)
      }
    }

    AsyncFunction("update") { (state: ActivityStateRecord) async in
      guard #available(iOS 17.2, *) else { return }
      for activity in liveActivities() {
        await activity.update(state.content)
      }
    }

    AsyncFunction("end") { () async in
      guard #available(iOS 17.2, *) else { return }
      for activity in Activity<OctetActivity>.activities {
        await activity.end(nil, dismissalPolicy: .immediate)
      }
    }

    // The app's cleaned and bounded {"<host>/<raw>": {name, command, approval}} as JSON. Anything
    // larger or of another shape is refused, and what the widget had stays as it was.
    AsyncFunction("setEntries") { (json: String) throws in
      guard json.utf8.count <= entriesByteLimit,
        let entries = try? JSONSerialization.jsonObject(with: Data(json.utf8)) as? [String: Any],
        entries.count <= entriesLimit,
        entries.values.allSatisfy({ value in
          guard let entry = value as? [String: Any] else { return false }
          return entry.values.allSatisfy { $0 is String }
        })
      else {
        throw InvalidEntriesException()
      }
      let defaults = UserDefaults(suiteName: appGroup)
      defaults?.set(json, forKey: entriesKey)
    }

    OnStartObserving("pushToStartToken") {
      guard #available(iOS 17.2, *) else { return }
      self.track("start") { key, id in
        Task { [weak self] in
          for await data in Activity<OctetActivity>.pushToStartTokenUpdates {
            if Task.isCancelled { break }
            self?.sendEvent("pushToStartToken", ["token": hex(data)])
          }
          self?.forget(key, id)
        }
      }
    }

    OnStopObserving("pushToStartToken") {
      self.cancel { $0 == "start" }
    }

    OnStartObserving("pushToken") {
      guard #available(iOS 17.2, *) else { return }
      self.track("activities") { key, id in
        Task { [weak self] in
          for await activity in Activity<OctetActivity>.activityUpdates {
            if Task.isCancelled { break }
            self?.watch(activity)
          }
          self?.forget(key, id)
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

  /// Reports a live activity's push token as it changes, and null once it is over. An activity that
  /// has already ended gets its null at once and no watching.
  @available(iOS 17.2, *)
  private func watch(_ activity: Activity<OctetActivity>) {
    guard activity.activityState == .active || activity.activityState == .stale else {
      sendEvent("pushToken", ["token": nil])
      return
    }
    track("token:" + activity.id) { key, id in
      Task { [weak self] in
        for await data in activity.pushTokenUpdates {
          if Task.isCancelled { break }
          self?.sendEvent("pushToken", ["token": hex(data)])
        }
        self?.forget(key, id)
      }
    }
    track("state:" + activity.id) { key, id in
      Task { [weak self] in
        for await state in activity.activityStateUpdates {
          if Task.isCancelled { break }
          if state == .ended || state == .dismissed {
            self?.sendEvent("pushToken", ["token": nil])
            break
          }
        }
        self?.forget(key, id)
      }
    }
  }

  /// Starts the task for `key` unless one runs already. `make` gets the key and the task's id; the
  /// lock is held until the task is stored, so its `forget` can never run first.
  private func track(_ key: String, _ make: (String, UUID) -> Task<Void, Never>) {
    lock.lock()
    defer { lock.unlock() }
    if tasks[key] == nil {
      let id = UUID()
      tasks[key] = (id, make(key, id))
    }
  }

  /// A task whose loop ended removes its own key, and not a newer task's under the same key.
  private func forget(_ key: String, _ id: UUID) {
    lock.lock()
    defer { lock.unlock() }
    if tasks[key]?.id == id {
      tasks[key] = nil
    }
  }

  private func cancel(_ matching: (String) -> Bool) {
    lock.lock()
    defer { lock.unlock() }
    for (key, entry) in tasks where matching(key) {
      entry.task.cancel()
      tasks[key] = nil
    }
  }

  /// While JavaScript listens, a Darwin notification from DenyIntent drains the App Group's denies;
  /// starting to listen drains them too, so a Deny tapped before the app was up is not lost. The
  /// observer is added and removed under the lock, so two calls can't race.
  private func hearDenies(_ on: Bool) {
    lock.lock()
    defer { lock.unlock() }
    guard on != hearingDenies else { return }
    hearingDenies = on
    let center = CFNotificationCenterGetDarwinNotifyCenter()
    let observer = Unmanaged.passUnretained(self).toOpaque()
    if on {
      CFNotificationCenterAddObserver(
        center, observer,
        { _, observer, _, _, _ in
          guard let observer else { return }
          Unmanaged<OctetLiveModule>.fromOpaque(observer).takeUnretainedValue().scheduleDrain()
        },
        denyNotification as CFString, nil, .deliverImmediately)
      scheduleDrain()
    } else {
      CFNotificationCenterRemoveObserver(
        center, observer, CFNotificationName(denyNotification as CFString), nil)
    }
  }

  private func scheduleDrain() {
    denyQueue.async { [weak self] in self?.drainDenies() }
  }

  /// Emits every waiting `deny.<uuid>` younger than `denyMaxAge`, then removes that one key. Each
  /// Deny has a key of its own, so there is no shared list to read, change and write back.
  private func drainDenies() {
    guard let defaults = UserDefaults(suiteName: appGroup) else { return }
    let now = Date().timeIntervalSince1970
    for key in defaults.dictionaryRepresentation().keys where key.hasPrefix(denyPrefix) {
      if let deny = defaults.dictionary(forKey: key),
        let mac = deny["mac"] as? String,
        let session = deny["session"] as? String,
        let approval = deny["approval"] as? String,
        let at = deny["at"] as? Double,
        abs(now - at) <= denyMaxAge
      {
        sendEvent("deny", ["mac": mac, "session": session, "approval": approval])
      }
      defaults.removeObject(forKey: key)
    }
  }
}
