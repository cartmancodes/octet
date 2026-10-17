// The contract test runs the real `octet hub` binary, so it is not part of `npm test` (which must
// stay hermetic). Run it with `npm run contract` after `cargo build -p octet`.
module.exports = {
  preset: "jest-expo",
  rootDir: "..",
  testEnvironment: "node",
  testMatch: ["<rootDir>/contract/**/*.contract.test.ts"],
  testTimeout: 30_000,
};
