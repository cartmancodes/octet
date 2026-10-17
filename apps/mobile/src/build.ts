// Which kind of build this is. A development build runs whatever JavaScript it is pointed at (a
// dev-launcher link, a Metro bundle over Wi-Fi), and that code holds the device token: it must
// not pair with the Mac the user works on. Daily use is the `preview` build (eas.json).

/** Read at each call, not at import: tests switch it. */
export const isDevBuild = (): boolean => typeof __DEV__ !== "undefined" && __DEV__;
