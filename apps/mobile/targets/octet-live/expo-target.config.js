// The Live Activity's widget extension, linked into the Xcode project by @bacons/apple-targets at
// prebuild. Files in _shared are compiled into the app as well (DenyIntent runs in the app's process).
/** @type {import('@bacons/apple-targets/app.plugin').Config} */
module.exports = {
  type: "widget",
  name: "OctetLiveWidget",
  displayName: "Octet",
  // pushToStartTokenUpdates and LiveActivityIntent buttons need iOS 17.2; app.json matches it.
  deploymentTarget: "17.2",
  entitlements: {
    "com.apple.security.application-groups": ["group.dev.octet.mobile"],
  },
};
