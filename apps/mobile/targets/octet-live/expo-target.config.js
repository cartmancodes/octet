// The Live Activity's widget extension, linked into the Xcode project by @bacons/apple-targets at
// prebuild. Files in _shared are compiled into the app as well (DenyIntent runs in the app's process).
/** @type {import('@bacons/apple-targets/app.plugin').Config} */
module.exports = {
  type: "widget",
  name: "OctetLiveWidget",
  displayName: "Octet",
  // The Island needs iOS 17.2 (push-to-start tokens). The app itself stays on 16.4; an extension
  // may require a newer iOS than its app, and the module checks availability at run time.
  deploymentTarget: "17.2",
  entitlements: {
    "com.apple.security.application-groups": ["group.dev.octet.mobile"],
  },
};
