// The contract test runs the real `octet hub` and `octet-relay` binaries, so it is not part of
// `npm test` (which must stay hermetic). Run it with `npm run contract` after
// `cargo build -p octet -p octet-relay`.
const preset = require("jest-expo/jest-preset");

/** jest-expo's own patterns, with `@noble` (ESM only, as the app's jest config has it) among the
 * packages it transforms. */
const transformIgnorePatterns = preset.transformIgnorePatterns.map((pattern) =>
  pattern.startsWith("/node_modules/(?!(") ? pattern.replace(/\)\)$/, "|@noble))") : pattern,
);

module.exports = {
  preset: "jest-expo",
  rootDir: "..",
  testEnvironment: "node",
  testMatch: ["<rootDir>/contract/**/*.contract.test.ts"],
  testTimeout: 30_000,
  transformIgnorePatterns,
};
