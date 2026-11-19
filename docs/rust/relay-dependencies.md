# Relay link dependencies

The phone link goes through a hosted relay and is end-to-end encrypted with Noise (see
`docs/superpowers/specs/2026-11-19-relay-connectivity-design.md`, sections 3 and 5). The hub
needs four things it did not have: a Noise implementation, Ed25519, a TLS client for outbound
`wss://`, and the root certificates to check the relay with. This page lists what was added to
`crates/octet-hub/Cargo.toml`, why each feature is on, and every crate that is new to
`Cargo.lock` with its licence. The licences are read from `cargo metadata --format-version 1`;
`deny.toml` carries one commented exception for each crate that needs one.

## Direct dependencies

Each is pinned to a full `x.y.z` version, as `getrandom = "0.4.3"` is, and `Cargo.lock` holds the
exact build (CI builds with `--locked`). All default features are off.

| Dependency | Pin | Features | Why |
| --- | --- | --- | --- |
| `snow` | 0.10.0 | `use-chacha20poly1305`, `use-sha2`, `use-curve25519`, `use-getrandom` | `Noise_IK_25519_ChaChaPoly_SHA256`: ChaChaPoly, SHA-256, X25519 and the random source for the ephemeral key. The default resolver also offers AES-GCM and BLAKE2; this pattern uses neither, so they stay off. Not `std`: snow's `std` feature switches on `ring` inside snow, and `snow::Error` implements `core::error::Error` without it. |
| `ed25519-dalek` | 3.0.0 | `zeroize` | The hub signs the relay's challenge (`"octet-relay-v1"` and 32 bytes); the relay verifies it. Signing and verifying need no feature. `zeroize` wipes the secret key on drop. Off: `fast` (precomputed tables, only a speed-up), `rand_core` (the hub makes the key from `getrandom` bytes with `SigningKey::from_bytes`), `pkcs8`, `pem`, `serde`, `batch`. |
| `tokio-tungstenite` | 0.30.0 (unchanged) | `connect`, `rustls-tls-webpki-roots` | The outbound `wss://` client. `connect` moves out of dev-dependencies into the normal ones (it brings `stream` and `tokio/net`; `handshake` comes with it, so the old `handshake` entry is gone). `rustls-tls-webpki-roots` turns on `tokio-rustls`, `rustls` and `webpki-roots`. The version did not change. The dev-dependency entry is removed because the normal one now covers it. |
| `rustls` | 0.23.46 | `ring`, `std`, `tls12` | Not called yet. `tokio-tungstenite` takes `rustls` with default features off, so no crypto provider is compiled in; this entry names one (see below). `std` is needed by `tokio-rustls`; `tls12` lets a self-hosted relay behind an older proxy work and adds no crates. Off: `aws_lc_rs`, `logging`, `prefer-post-quantum`, `brotli`, `zlib`, `fips`. |

## Crypto provider: `ring`, not `aws-lc-rs`

rustls needs one provider for its primitives. `aws-lc-rs` is the rustls default but its build
needs `cmake` and a C++ toolchain (it compiles AWS-LC through `aws-lc-sys`), and it brings
licences that are not on the list today (OpenSSL, ISC and more). `ring` needs only the C
compiler every Rust target already has, its licence is `Apache-2.0 AND ISC` (ISC is the one new
licence), and it is the provider the webpki crates are built against. No other C or C++ build
step is added to the tree.

Cost: on a build that targets Linux from a Mac, `ring`'s build script compiles C and assembly for
the target and needs that target's C headers. See "Linux checks" below.

## What `snow` pulls in

With the features above `snow` uses RustCrypto's `chacha20poly1305` and `sha2` (the 0.10 line,
which differs from the 0.11 `sha2` the hub already uses), `curve25519-dalek` 4.1 for X25519,
and `getrandom` 0.3. All pure Rust. `curve25519-dalek` is therefore in the tree twice (4.1.3
through `snow`, 5.0.0 through `ed25519-dalek` 3), as are `sha2`, `fiat-crypto` and `getrandom`.
`deny.toml` warns on duplicates; it does not fail.

## Crates new to `Cargo.lock`

50 package versions. "Needs a licence entry" lists the crates whose SPDX expression has no
alternative already allowed in `deny.toml`.

### Through `snow` (Noise)

| Crate | Version | Licence | Why |
| --- | --- | --- | --- |
| `snow` | 0.10.0 | Apache-2.0 OR MIT | Noise implementation |
| `chacha20poly1305` | 0.10.1 | Apache-2.0 OR MIT | ChaChaPoly cipher |
| `chacha20` | 0.9.1 | Apache-2.0 OR MIT | stream cipher under it |
| `poly1305` | 0.8.0 | Apache-2.0 OR MIT | MAC under it |
| `aead` | 0.5.2 | MIT OR Apache-2.0 | AEAD traits |
| `cipher` | 0.4.4 | MIT OR Apache-2.0 | cipher traits |
| `inout` | 0.1.4 | MIT OR Apache-2.0 | buffers for `cipher` |
| `universal-hash` | 0.5.1 | MIT OR Apache-2.0 | traits for `poly1305` |
| `opaque-debug` | 0.3.1 | MIT OR Apache-2.0 | `Debug` for `poly1305` |
| `crypto-common` | 0.1.7 | MIT OR Apache-2.0 | shared crypto traits (0.1 line) |
| `generic-array` | 0.14.7 | MIT | array type under the 0.1 traits |
| `version_check` | 0.9.5 | MIT/Apache-2.0 | `generic-array` build script |
| `block-buffer` | 0.10.4 | MIT OR Apache-2.0 | `digest` 0.10 |
| `digest` | 0.10.7 | MIT OR Apache-2.0 | `sha2` 0.10 |
| `sha2` | 0.10.9 | MIT OR Apache-2.0 | SHA-256 for Noise |
| `cpufeatures` | 0.2.17 | MIT OR Apache-2.0 | CPU detection for the ciphers and `curve25519-dalek` 4 |
| `curve25519-dalek` | 4.1.3 | **BSD-3-Clause** | X25519 |
| `fiat-crypto` | 0.2.9 | MIT OR Apache-2.0 OR BSD-1-Clause | field arithmetic for it |
| `getrandom` | 0.3.4 | MIT OR Apache-2.0 | ephemeral key randomness |
| `r-efi` | 5.3.0 | MIT OR Apache-2.0 OR LGPL-2.1-or-later | `getrandom` 0.3 on UEFI only |
| `wasip2` | 1.0.4+wasi-0.2.12 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | `getrandom` 0.3 on WASI only |
| `wit-bindgen` | 0.57.1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | under `wasip2` |

`r-efi`, `wasip2` and `wit-bindgen` are listed by `cargo metadata` but are not built on any
target the hub ships for.

### Through `ed25519-dalek` (Ed25519)

| Crate | Version | Licence | Why |
| --- | --- | --- | --- |
| `ed25519-dalek` | 3.0.0 | **BSD-3-Clause** | Ed25519 sign and verify |
| `ed25519` | 3.0.0 | Apache-2.0 OR MIT | signature types |
| `curve25519-dalek` | 5.0.0 | **BSD-3-Clause** | curve arithmetic (5.x line) |
| `curve25519-dalek-derive` | 0.1.1 | MIT/Apache-2.0 | proc macro for both `curve25519-dalek` versions |
| `fiat-crypto` | 0.3.0 | MIT OR Apache-2.0 OR BSD-1-Clause | field arithmetic for it |

The rest of its tree (`digest` 0.11, `sha2` 0.11, `signature`, `subtle`, `zeroize`, `rand_core`)
was already in the lock.

### Through `rustls` and `tokio-tungstenite` (TLS client)

| Crate | Version | Licence | Why |
| --- | --- | --- | --- |
| `rustls` | 0.23.46 | Apache-2.0 OR ISC OR MIT | TLS client; satisfied by MIT or Apache-2.0 |
| `rustls-pki-types` | 1.15.1 | MIT OR Apache-2.0 | certificate and name types |
| `rustls-webpki` | 0.103.15 | **ISC** | certificate verification |
| `tokio-rustls` | 0.26.6 | MIT OR Apache-2.0 | rustls over tokio streams |
| `once_cell` | 1.21.4 | MIT OR Apache-2.0 | used by rustls |
| `ring` | 0.17.14 | Apache-2.0 **AND ISC** | crypto provider |
| `untrusted` | 0.9.0 | **ISC** | byte reader for `ring` and webpki |
| `cc` | 1.11.2 | MIT OR Apache-2.0 | `ring` build script (build-dependency) |
| `shlex` | 2.0.1 | MIT OR Apache-2.0 | `cc` |
| `find-msvc-tools` | 0.1.15 | MIT OR Apache-2.0 | `cc`, Windows only |
| `getrandom` | 0.2.17 | MIT OR Apache-2.0 | `ring` randomness |
| `windows-sys` | 0.52.0 | MIT OR Apache-2.0 | `ring` on Windows only |
| `windows-targets` and eight `windows_*` crates | 0.52.6 | MIT OR Apache-2.0 | under it, Windows only |
| `webpki-roots` | 1.0.9 | **CDLA-Permissive-2.0** | Mozilla roots, compiled in |
| `webpki-roots` | 0.26.11 | **CDLA-Permissive-2.0** | the version `tokio-tungstenite` asks for; re-exports 1.0.9 |

The eight `windows_*` crates are `windows_aarch64_gnullvm`, `windows_aarch64_msvc`,
`windows_i686_gnu`, `windows_i686_gnullvm`, `windows_i686_msvc`, `windows_x86_64_gnu`,
`windows_x86_64_gnullvm` and `windows_x86_64_msvc`.

## `deny.toml`

Each licence is an `exceptions` entry for the crate that needs it, with a comment, so the global
`allow` list is unchanged and a different crate carrying the same licence still fails the check:

- BSD-3-Clause: `curve25519-dalek`, `ed25519-dalek` (beside the existing `subtle` entry).
- ISC: `ring`, `rustls-webpki`, `untrusted`. `ring` is `Apache-2.0 AND ISC`, so Apache-2.0 (already
  allowed) and the new ISC entry are both needed.
- CDLA-Permissive-2.0: `webpki-roots` (both versions).

`rustls` is `Apache-2.0 OR ISC OR MIT`, so it passes on MIT without an entry. `fiat-crypto` and
`r-efi` pass on their MIT alternative.

No licence outside ISC, BSD-3-Clause and CDLA-Permissive-2.0 is new. `cargo-deny` is not
installed locally; CI runs it. If it differs from the SPDX strings above (it reads the same
`license` field), that is the place to look.

## Linux checks

Nothing in the workspace compiled C before. `ring` does, so the cross-lint
`cargo clippy --target x86_64-unknown-linux-gnu` from a Mac now fails in `ring`'s build script
(`failed to find tool "x86_64-linux-gnu-gcc"`; with `CC=clang` it finds no `assert.h`) unless a
Linux C toolchain and headers are installed. Lint and test for Linux in the Docker image instead:

```sh
docker run --rm --init -v "$PWD":/w -w /w -v octet-linux-target:/target \
  -v octet-linux-cargo:/usr/local/cargo/registry -e CARGO_TARGET_DIR=/target rust:1.98.1 \
  bash -c 'rustup component add clippy; cargo clippy --locked --workspace --all-targets -- -D warnings'
```

That passes (aarch64 Linux on Apple silicon). GitHub's `ubuntu-latest` runner builds `ring`
natively and needs nothing extra.
