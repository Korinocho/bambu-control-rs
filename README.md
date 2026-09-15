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

Ports used on the printer: `8883` (MQTT over TLS), `990` (implicit FTPS) and the camera
port.

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

## Security notes

- The LAN access code is stored in plain text in `config.toml`.
- The MQTT connection uses TLS, but the printer's self-signed certificate is not
  verified (see `src/mqtt.rs`). This is a LAN-only design; treat it accordingly.
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

Binary releases link many third-party Rust crates and embed fonts; their license texts
are included in the release archive.

`vendor/suppaftp` is a copy of the suppaftp crate (MIT OR Apache-2.0, license files
included) with one small patch, described in `vendor/suppaftp/PATCHES.md`.
