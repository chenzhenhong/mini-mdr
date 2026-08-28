# mini-mdr

[中文](README_zh.md)

`mini-mdr` is a tray-only, cross-platform UPnP/DLNA Digital Media Renderer
(DMR). It receives playback commands from a phone or desktop control point and
plays audio or video through a replaceable player backend.

The default player backend is an external [`mpv`](https://mpv.io/) process
controlled through mpv's JSON IPC interface.

## Features

- Windows, macOS, and Linux target design
- Audio and video playback URLs
- UPnP/DLNA MediaRenderer device description
- SSDP discovery (`M-SEARCH` responses, periodic `ssdp:alive`, `ssdp:byebye` on stop)
- `ConnectionManager` with protocol info and connection queries
- `AVTransport` with `SetAVTransportURI`, `Play`, `Pause`, `Stop`, `Seek`
  (`REL_TIME`), `GetTransportInfo`, `GetPositionInfo`, `GetMediaInfo`
- `RenderingControl` with master-channel volume and mute
- GENA event subscriptions with renewal, expiry, and `LastChange` notifications
- Pluggable `PlayerBackend` abstraction
- Default `mpv` backend over JSON IPC (named pipe on Windows, Unix socket
  elsewhere); the mpv process starts lazily on first media load
- Optional `VLC` backend over HTTP interface
- System-tray-only application control with a dynamic Start/Stop Cast label
- Local settings server with an editable form that persists configuration
- Media history tracking with configurable max entries (persisted to disk)
- Signal handling for clean shutdown (SIGINT, SIGHUP, SIGTERM on Unix)
- Auto-start cast on launch
- Embedded 32x32 RGBA8 tray icon with no runtime image decoding
- i18n support: English and Chinese, auto-detected from system locale

The tray menu has the following structure:

1. `Start Cast` / `Stop Cast` (checkbox reflecting current Cast state)
2. `More` > (submenu)
   - `Open Settings`
   - `Open Directory` (opens the config/log directory in the file manager)
3. `Quit`

## Architecture

```text
DMC: phone / desktop controller
    │ SSDP, SOAP, GENA
    ▼
mini-mdr
    ├── SSDP discovery
    ├── UPnP HTTP and SOAP services
    ├── renderer state
    ├── player backend abstraction
    └── system tray
            │
            ▼
        mpv JSON IPC
            │
            ├── video output
            └── audio output
```

The DMC sends control commands to `mini-mdr`. The DMR then gives the media URL
to the player backend. The player, not the DMC, retrieves the actual media from
the DMS over HTTP.

```text
DMC ── SOAP control ──> mini-mdr ── IPC ──> mpv
DMS ── HTTP media ───────────────────────> mpv
```

## Requirements

- Rust stable toolchain
- A desktop environment with a system tray
- `mpv` installed and available in `PATH`, unless a custom path is configured
- A UPnP/DLNA control point on the same local network

On Windows, use the MSVC Rust toolchain. On Linux, the desktop environment must
provide a tray-compatible notification area or StatusNotifier host. Headless
environments may run the non-tray protocol components, but tray creation is
expected to fail gracefully.

## Running

Install `mpv`, then run:

```text
cargo run
```

The application starts with Cast services and the settings HTTP server enabled.
The settings server binds to `127.0.0.1:7878` (or an ephemeral fallback port if
busy) and opens the page with the system default browser when `Open Settings` is
selected.

## Runtime Behavior

### Start / Stop Cast

`Start Cast` starts the UPnP HTTP server and SSDP discovery service. It also
creates the configured player backend. Once running, compatible DMC clients can
discover and control the renderer.

`Stop Cast` stops the current player, releases the player backend, and stops the
Cast services. It does not stop the settings server if that server was already
started.

### Open Settings

The settings server starts automatically on launch:

```text
Application startup       -> bind 127.0.0.1:7878
Port conflict             -> select an ephemeral fallback port
Open Settings click       -> open the page in the default browser
```

The server listens only on loopback and opens the page with the system default
browser. Current settings include the device name, player backend, mpv path,
and vlc path.

### Quit

`Quit` stops Cast services, stops the player process, closes the settings
server when present, and terminates the application.

## Configuration

The default configuration is:

```toml
[device]
name = "mini-mdr(<hostname>)"   # hostname from $HOSTNAME or $HOST

[player]
backend = "mpv"
mpv_path = "mpv"
vlc_path = "vlc"

[settings]
port = 7878
max_history = 200
```

Device name includes the system hostname by default (e.g., `mini-mdr(laptop)`).
History entries are persisted to `history.json` in the same config directory.

The configuration is loaded from the platform-specific user configuration
directory provided by the `directories` crate. If no file exists, these
defaults are used.

## Player Backends

The protocol layer depends on the `PlayerBackend` trait rather than directly on
mpv:

```text
PlayerBackend
    ├── MpvBackend (default)
    └── VlcBackend
```

The interface currently covers loading, play, pause, stop, seek, volume, mute,
status queries, and protocol info reporting. Future backends such as GStreamer
or FFmpeg should implement this trait and be registered in
`player::create_backend`.

The current plugin model is compile-time extensibility through Rust
implementations selected by configuration. Runtime loading of third-party DLLs
or shared libraries is not implemented.

Device and service descriptions are maintained as standalone files under
`resources/` and embedded into the executable at compile time with
`include_str!`. Deployment therefore does not require separate XML files.

The editable tray artwork is `resources/icon.png`. Before committing an icon
change, convert it offline to `resources/icon.rgba`: 32x32, row-major,
non-premultiplied RGBA8 (4096 bytes). The executable embeds the raw bytes with
`include_bytes!` and passes them directly to `ldtray`; no image decoder or
startup transformation is involved.

## UPnP Scope

The current implementation provides a deliberately small protocol surface:

- `device.xml` plus one SCPD file per service, embedded at compile time
- SSDP discovery with standard search targets and lifecycle announcements
- SOAP control for transport, position/media queries, seek, volume, and mute
- Standard UPnP error responses with error codes
- GENA `SUBSCRIBE`/`UNSUBSCRIBE` with SID renewal, timeout clamping, expiry
  cleanup, and `LastChange` notifications to subscriber callbacks

The implementation is not yet a complete DLNA certification implementation. In
particular, requests are served sequentially, track metadata is minimal
(no DIDLLite), and only instance `0` exists.

## Development

Format and verify the project with:

```text
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

Before testing playback, verify that mpv is available:

```text
mpv --version
```

Useful manual checks:

1. Start the application and confirm the settings port is listening on `127.0.0.1`.
2. Click `Open Settings` and confirm a browser opens on `127.0.0.1`.
3. Click `Start Cast` and send an SSDP `M-SEARCH` request.
4. Fetch the returned `device.xml` URL.
5. Use a compatible DMC to send `SetAVTransportURI` and `Play`.
6. Stop Cast and confirm the UPnP and SSDP services are released.

## Project Status

This is an early MVP under active development. Current status:

- Core UPnP/DLNA protocol implemented (AVTransport, ConnectionManager, RenderingControl)
- SSDP discovery and lifecycle management working
- mpv backend with JSON IPC control
- System tray integration with start/stop cast, settings, and quit
- Settings page with configuration editing and media history
- History persistence to disk with configurable max entries
- Signal handling for clean shutdown on Unix
- AUR packaging for Arch Linux

Priority follow-up work: concurrent request handling, persistent per-install
UPnP UDN, DIDLLite metadata, and integration tests.
