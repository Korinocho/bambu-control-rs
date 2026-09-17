# Bambu Control

Unofficial Windows desktop app to monitor and control Bambu Lab 3D printers over your
local network. Written in Rust with [egui](https://github.com/emilk/egui).

Everything runs on your LAN: the app talks directly to the printer over MQTT, FTPS and
the camera stream. No cloud account is required to use it.

## Disclaimer

Bambu Control is an independent, unofficial open-source project. It is not affiliated
with, endorsed by, sponsored by, or supported by Bambu Lab or any of its affiliates.
"Bambu Lab", "Bambu Studio", "Bambu Handy", "AMS", and printer model names such as X1,
P1S and A1 are trademarks of their respective owners. They are used here only to
identify compatible hardware. This software controls printers on your local network
through LAN / Developer Mode, which Bambu Lab does not officially support; you use it at
your own risk, including any effect on your printer or its warranty. Printer error (HMS)
descriptions and firmware version information are fetched at runtime from Bambu Lab's
servers, belong to Bambu Lab, and are not distributed with this project.
The `BBL CA` certificate embedded in the app belongs to Bambu Lab, is included only to
verify printers, and is not covered by this project's license.

## Features

- Live printer status over MQTT (LAN mode): temperatures, progress, current job, AMS.
- Camera stream from the printer.
- Job files over FTPS: sliced `.3mf` metadata, plate thumbnails and object boxes.
- HMS error code lookup, so errors show a description instead of a number.
- Firmware version information.

## Requirements

- Windows 10 or 11.
- The printer on the same local network, with **LAN Mode / Developer Mode** enabled.
- The printer's **IP address**, **serial number** and **LAN access code**
  (printer screen: Settings → Network).

Ports used on the printer: `8883` (MQTT over TLS), `990` (implicit FTPS) and `6000`
(camera).

## Build

Requires a Rust toolchain with the 2024 edition.

```sh
cargo build --release
```

The binary is written to `target/release/`.

## Configuration

The app stores its configuration in `config.toml`, next to the executable. It contains
each printer's IP, serial and LAN access code **in plain text**.

That file is in `.gitignore` and must never be committed or shared, and release archives
must not be built straight out of `target/release/`, because the config sits next to the
executable there.

### Where files go

| What | Where |
|---|---|
| Timelapses and recordings you play, and the app's own listings and thumbnails | `%LOCALAPPDATA%\Bambu Control\cache` |
| **Save to PC** | `Downloads\Bambu Control\<printer name>` |

The cache has a size cap, 5 GB by default, set with `cache_cap_gb` under `[files]` in
`config.toml`. When it is full the least recently used files go first, except a file the
player has open. **Clear cache** in the files view empties it, again skipping anything
open. Files you saved with "Save to PC" are yours: nothing in the app ever deletes them.

Downloads have no resume — the printers answer `502` to `REST` — so a cancelled or
interrupted download leaves nothing behind, and starting it again starts from zero.

## Security notes

Each connection to the printer is treated separately.

- **FTPS, port 990 (file browsing):** verified. The printer's certificate must be issued
  by Bambu Lab's `BBL CA`, which is embedded in the app, and its subject must be the
  serial you configured for that printer; the handshake itself must be signed with that
  certificate's key. Anything else is refused before the access code is sent, and the app
  offers no way to trust a refused printer. Certificate expiry is deliberately not
  checked, because Bambu's CA expires in 2032 and the printers' certificates in 2035.
- **Camera, port 6000:** verified, by the same embedded `BBL CA`, the same configured
  serial and the same handshake signature check as FTPS above. The camera's first bytes
  after the handshake carry the access code, so a certificate that is refused leaves them
  unsent, and the tests assert that nothing above the handshake ever reaches a printer
  whose certificate was refused.
- **MQTT, port 8883:** **not verified yet.** It accepts any certificate, so the access
  code is sent to whatever answers at the printer's address. Tracked as issue #1; until
  it lands, treat MQTT as LAN-only.
- The LAN access code is stored in plain text in `config.toml`.
- Outbound connections: your printer on the LAN, plus Bambu Lab servers for HMS error
  descriptions and firmware version information.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for
inclusion in this project by you, as defined in the Apache-2.0 license, shall be dual
licensed as above, without any additional terms or conditions.

Binary releases link many third-party Rust crates and embed fonts.

`vendor/suppaftp` is a copy of suppaftp 10.0.2 by Christian Visintin
(<https://github.com/veeso/suppaftp>), MIT OR Apache-2.0, with its license files
included and one small patch, described in `vendor/suppaftp/PATCHES.md`.
