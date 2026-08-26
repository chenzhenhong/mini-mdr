# mini-mdr

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
- System-tray-only application control with a dynamic Start/Stop Cast label
- Lazy local settings server with an editable form that persists configuration

The tray menu contains exactly three entries:

1. `开始 Cast` / `停止 Cast` (label reflects current Cast state)
2. `打开设置`
3. `退出程序`

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

The application starts with the tray only. It does not start the settings HTTP
server or Cast services until their corresponding menu action is selected.

## Runtime Behavior

### Start / Stop Cast

`开始 Cast` starts the UPnP HTTP server and SSDP discovery service. It also
creates the configured player backend. Once running, compatible DMC clients can
discover and control the renderer.

`停止 Cast` stops the current player, releases the player backend, and stops the
Cast services. It does not stop the settings server if that server was already
started.

### Open Settings

The settings server is deliberately lazy:

```text
Application startup       -> no settings listener
First Open Settings click -> bind 127.0.0.1:7878
Port conflict             -> select an ephemeral fallback port
Later clicks              -> reuse the same server
```

The server listens only on loopback and opens the page with the system default
browser. Current settings include the device name, player backend, and mpv path.

### Quit

`退出程序` stops Cast services, stops the player process, closes the settings
server when present, and terminates the application.

## Configuration

The default configuration is:

```toml
[device]
name = "mini-mdr"

[player]
backend = "mpv"
mpv_path = "mpv"

[settings]
port = 7878
```

The configuration is loaded from the platform-specific user configuration
directory provided by the `directories` crate. If no file exists, these
defaults are used.

## Player Backends

The protocol layer depends on the `PlayerBackend` trait rather than directly on
mpv:

```text
PlayerBackend
    └── MpvBackend (default)
```

The interface currently covers loading, play, pause, stop, seek, volume, mute,
and status queries. Future backends such as GStreamer or FFmpeg should implement
this trait and be registered in `player::create_backend`.

The current plugin model is compile-time extensibility through Rust
implementations selected by configuration. Runtime loading of third-party DLLs
or shared libraries is not implemented.

Device and service descriptions are maintained as standalone files under
`resources/` and embedded into the executable at compile time with
`include_str!`. Deployment therefore does not require separate XML files.

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
(no DIDLLite), only instance `0` exists, and the advertised protocol info list
is static rather than derived from the active player backend.

## Development

Format and verify the project with:

```text
cargo fmt -- --check
cargo check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

Before testing playback, verify that mpv is available:

```text
mpv --version
```

Useful manual checks:

1. Start the application and confirm that no settings port is listening.
2. Click `打开设置` and confirm a browser opens on `127.0.0.1`.
3. Click `开始 Cast` and send an SSDP `M-SEARCH` request.
4. Fetch the returned `device.xml` URL.
5. Use a compatible DMC to send `SetAVTransportURI` and `Play`.
6. Stop Cast and confirm the UPnP and SSDP services are released.

## Project Status

This is an early MVP and the code should be treated as active development. The
highest-priority follow-up work is to connect all standard AVTransport query
actions, implement GENA, improve HTTP/SSDP lifecycle handling, add configuration
editing, and add automated protocol tests.
