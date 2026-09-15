# Vendored suppaftp 10.0.2: local patches

This directory is suppaftp 10.0.2 exactly as published on crates.io, with one
change. The app uses it through `[patch.crates-io]` in the top-level
`Cargo.toml`.

- Source: the crates.io package `suppaftp-10.0.2`, upstream commit
  `194bdd1979b16c4848d1fad6897dfa524b688d88` (`crates/suppaftp`; see
  `.cargo_vcs_info.json`).
- Licence: MIT OR Apache-2.0, as upstream. The package on crates.io does not
  include licence files, so `LICENSE-MIT` and `LICENSE-APACHE` were copied from
  the root of the upstream repository at that commit.
- Not copied: `.cargo-ok`, cargo's registry extraction marker.
- `Cargo.lock` and `Cargo.toml.orig` are upstream's, kept unchanged. Cargo
  ignores both here.

## Integrity

A `[patch.crates-io]` path dependency has no checksum in `Cargo.lock`, so Cargo
would build any later edit under this directory silently. CI therefore checks
the directory against the published crate
(`.github/scripts/check-vendored-suppaftp.sh`, step "Vendored suppaftp"):
- it downloads `suppaftp-10.0.2.crate` from static.crates.io and requires the
  crates.io index checksum
  `821001051ea3d12a60fb790b8c7cb9a6f5f8698dcfdca4cd533a025fefb0b5b8`;
- every file of the crate must be here byte for byte, except `src/lib.rs`, and
  no file may be missing;
- `src/lib.rs` must differ from the crate's by exactly `vendor/suppaftp.patch`
  (`diff -u --label a/src/lib.rs --label b/src/lib.rs`);
- the only added files are `LICENSE-MIT` (SHA-256
  `4e883a0c89656afe3aaa559b3d4096a8aaa7534f00937f1c0054e231b2928078`),
  `LICENSE-APACHE` (SHA-256
  `c6596eb7be8581c18be736c846fb9173b69eccf6ef94c5135893ec56bd92ba08`) and this
  file.

The CI provider and danger greps, and the source-rule tests in
`tests/source_rules.rs`, scan `vendor/suppaftp/src` as well as `src`.
`.gitattributes` marks `vendor/**` as `-text`, so git never rewrites line
endings here.

A new patch updates `src/lib.rs` (or another file, and then the script),
`vendor/suppaftp.patch` and this file together.

## Patch 1: export the `TlsConnector` trait

`src/lib.rs`, after `pub use sync_ftp::DataStream;`:

```rust
// -- export secure (connector trait)
// bambu-control patch (vendor/suppaftp/PATCHES.md): lets applications
// implement their own TLS connector.
#[cfg(feature = "secure")]
#[cfg_attr(docsrs, doc(cfg(feature = "secure")))]
pub use sync_ftp::TlsConnector;
```

**Why.**
- suppaftp's public API takes `impl TlsConnector` (`connect_secure_implicit`, `into_secure`).
- But the trait is declared in the private module `sync_ftp`: `lib.rs` has `mod sync_ftp;`, and `sync_ftp/tls.rs` declares `pub trait TlsConnector`.
- The crate root re-exports only `TlsStream` and the ready-made connectors, so no application can implement a connector of its own.
- bambu-control needs one (design doc `docs/printer-files-design.md`, 5.2 and 5.3). Its `AnchoredConnector`:
  - sets socket timeouts before the TLS handshake;
  - drives the handshake;
  - records every connection's certificate refusal by type, before suppaftp flattens connector errors into `FtpError::SecureError(String)` or data-stream errors into `FtpError::BadResponse`.

**Scope.** This is a visibility change only. No code path, type or behaviour inside suppaftp changes:
- the trait is the one the crate already defines and uses;
- `RustlsConnector` and `RustlsStream` are untouched;
- the export uses the same `secure` feature gate as the trait itself.

## Dropping the patch

Remove this directory and the `[patch.crates-io]` entry once an upstream suppaftp release exports `TlsConnector` from the crate root, or otherwise lets an application supply its own connector. Then:
- bump the `suppaftp` version in `Cargo.toml`;
- run `cargo update -p suppaftp`;
- confirm `cargo test --locked` still passes.
