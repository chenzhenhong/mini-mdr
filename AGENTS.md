# mini-mdr Agent Guide

This file is project context for coding agents working on `mini-mdr`. Read it
before changing code.

## Project Identity

- Project name: `mini-mdr`
- Project type: Rust binary application
- Purpose: tray-only UPnP/DLNA Digital Media Renderer
- Supported media: audio and video
- Default player: external `mpv` through JSON IPC
- Intended platforms: Windows, macOS, and Linux
- Rust edition: 2024

`mdr` means Digital Media Renderer in this project. It does not mean Markdown
reader or Markdown renderer.

## Product Requirements

The application must remain tray-only for application control. The tray menu
has exactly these actions:

1. Start Cast / Stop Cast
2. Open Settings
3. Quit

Start/Stop Cast controls the DMR services as a group. It is not merely a local
playback toggle:

- Start Cast starts SSDP and UPnP HTTP services and creates the player backend.
- Stop Cast stops the current player, releases the player backend, and stops
  SSDP and UPnP services.
- Stop Cast must not stop an already-running settings server.

The application auto-starts cast on launch. The settings server is lazy and
must not start during application startup:

- The first Open Settings action starts it.
- It binds to `127.0.0.1:7878` when available.
- If the preferred port is unavailable, it chooses an ephemeral fallback port.
- Later Open Settings actions reuse the same server.
- It is shut down on application exit.

The application handles SIGINT, SIGHUP, and SIGTERM for clean shutdown on
Unix. On Windows, only SIGINT is handled (SIGHUP/SIGTERM are Unix-only).

The application may show an mpv video window during video playback. That does
not violate the tray-only requirement: mini-mdr itself has no control window.

All user-facing strings (tray menu labels, settings page HTML, state display
text, validation errors) must use `i18n::t(lang, "key")` backed by Fluent `.ftl`
files under `locales/`. Call `i18n::detect()` once at startup (already in
`main.rs`). Pass `Language` to functions that build user-facing text. Add new
keys to both `locales/en/main.ftl` and `locales/zh-CN/main.ftl`.

## Architecture

```text
main
  -> App
      -> tray command channel
      -> lazy settings server
      -> Cast lifecycle
          -> UpnpServer
          -> SsdpServer
          -> PlayerBackend
              -> MpvBackend
```

Important dependency direction:

- `tray` sends application commands; it must not implement UPnP behavior.
- `app` owns lifecycle and coordinates components.
- `upnp` owns HTTP device descriptions and SOAP dispatch.
- `ssdp` owns UDP discovery.
- `state` owns renderer state types and history entries.
- `player` owns the backend trait and concrete player implementations.
- UPnP service code should depend on `PlayerBackend`, not on `MpvBackend`.
- `MpvBackend` must not know about SOAP or UPnP XML.

## Current Files

```text
src/main.rs                 Entry point and module declarations
src/app.rs                  App lifecycle and tray command handling
src/config.rs               Config defaults, load, save, and history persistence
src/i18n.rs                 Language enum, locale detection, FluentBundle wrapper
src/log.rs                  Panic-free stderr logging for GUI subsystem
src/state.rs                Cast, transport, and renderer state types; HistoryEntry
src/tray.rs                 ldtray integration, three menu actions, signal handling
src/settings_server.rs      Lazy loopback settings HTTP server (settings + history UI)
src/ssdp.rs                 SSDP multicast listener and M-SEARCH response
src/upnp.rs                 UPnP HTTP descriptions and SOAP handling
src/player/mod.rs           PlayerBackend trait and backend factory
src/player/mpv.rs           External mpv JSON IPC backend
locales/en/main.ftl         English translations (Fluent format)
locales/zh-CN/main.ftl      Chinese translations (Fluent format)
resources/*.xml             UPnP device/service descriptions (embedded at compile time)
resources/icon.png          Editable tray icon source
resources/icon.rgba         Embedded 32x32 RGBA8 icon (4096 bytes)
aur/                        AUR packaging files (PKGBUILD, .SRCINFO, .desktop)
```

UPnP descriptions live in `resources/*.xml` and are embedded at compile time
with `include_str!`. Keep descriptive XML out of Rust source. Dynamic values use
explicit placeholders such as `{{DEVICE_NAME}}` and must be XML-escaped before
replacement. Do not add runtime resource-file loading unless explicitly
requested.

The editable tray artwork is `resources/icon.png`, but the application embeds
`resources/icon.rgba`: exactly 32x32 row-major, non-premultiplied RGBA8 (4096
bytes), which is the native `ldtray::Icon` input. Regenerate it offline when the
PNG changes; do not add runtime image decoding or resource lookup. With Pillow:
`Image.open("resources/icon.png").convert("RGBA").resize((32, 32),
Image.Resampling.LANCZOS).tobytes()` and write those bytes to `icon.rgba`.

The settings page is served from `resources/index.html` (opened via
`open::that("http://{address}/")`). All user-visible text is injected through
`{{KEY}}` templates resolved from `locales/*/main.ftl`. Brand imagery (nav logo,
About icon, favicon) is served from `resources/icon.png` via the `GET /icon.png`
route — do not reintroduce inline SVG placeholders for the app icon.

## Commit & Release Discipline

### Pre-commit (prevents CI failure)

- **Before committing any Rust change, run `cargo fmt --all` and include the
  formatted output in the commit.** CI runs `cargo fmt --all -- --check`; any
  formatting mismatch fails the pipeline with exit 1.
- `cargo fmt` only reformats and does **not** need the linker; it runs fine on
  this machine (which lacks MSVC `link.exe`).
- Do not commit any `.rs` file that has not been run through `cargo fmt --all`.
- Recommended flow: edit → `cargo fmt --all` → `git add -A` → `git commit` →
  `git push`.

### Release discipline

- **Do not** automatically bump the `Cargo.toml` version, create a tag, or
  trigger a release.
- Only when the user explicitly says "release一下" (release it): bump the
  `Cargo.toml` version to the next increment (current latest tag is `v0.2.18`),
  commit, create a `vX.Y.Z` tag, and `git push --tags`. CI builds the release
  automatically.
- For normal feature/fix work, commit and `git push origin main` without a tag.

### Environment note

- This machine (Windows) lacks the MSVC linker, so `cargo build` / `cargo check`
  fail locally; compilation is validated by CI (`windows-latest`).
- `cargo fmt` works locally — use it to self-check formatting before pushing.

## PlayerBackend Contract

`src/player/mod.rs` defines the current synchronous trait:

```rust
pub trait PlayerBackend: Send {
    fn load(&mut self, uri: &str) -> anyhow::Result<()>;
    fn play(&mut self) -> anyhow::Result<()>;
    fn pause(&mut self) -> anyhow::Result<()>;
    fn stop(&mut self) -> anyhow::Result<()>;
    fn seek(&mut self, position: Duration) -> anyhow::Result<()>;
    fn set_volume(&mut self, volume: u8) -> anyhow::Result<()>;
    fn set_mute(&mut self, muted: bool) -> anyhow::Result<()>;
    fn status(&mut self) -> anyhow::Result<PlayerStatus>;
}
```

When adding a backend:

1. Add a module under `src/player/`.
2. Implement `PlayerBackend`.
3. Register it in `create_backend`.
4. Keep UPnP code unchanged unless the standard capability list needs to be
   exposed by the trait.
5. Add backend unit tests for command mapping and error behavior.

The current `MpvBackend` starts mpv lazily on the first media load and talks
JSON IPC over a named pipe on Windows or a Unix domain socket elsewhere. The
endpoint is unique per process (`mini-mdr-<pid>-<nanos>`), startup waits at most
5 seconds and fails fast if mpv exits early, property reads tolerate
unavailable properties (idle state returns defaults), and dropping the session
kills the child and removes the socket file.

## UPnP/DLNA Scope

The intended protocol flow is:

```text
SSDP discovery (alive, M-SEARCH, byebye)
  -> GET device.xml
  -> GET service descriptions
  -> SUBSCRIBE for events
  -> SOAP SetAVTransportURI
  -> SOAP Play/Pause/Stop/Seek
  -> player fetches media URL from the DMS
  -> GENA NOTIFY LastChange on state changes
```

Implemented today:

- `device.xml` and per-service SCPDs in `resources/`, embedded via `include_str!`
- `ConnectionManager`: `GetProtocolInfo`, `GetCurrentConnectionIDs`,
  `GetCurrentConnectionInfo`
- `AVTransport`: `SetAVTransportURI`, `Play`, `Pause`, `Stop`, `Seek`
  (`REL_TIME` only), `GetTransportInfo`, `GetPositionInfo`, `GetMediaInfo`,
  `GetTransportSettings`; instance 0 only
- `RenderingControl`: master-channel `GetVolume`/`SetVolume`/`GetMute`/`SetMute`
- Standard UPnP fault responses with error codes (401/402/501/710/714/718)
- GENA: SID issuance, renewal via `SID` header, timeout clamping to 60..86400s,
  expiry cleanup, initial + change notifications with `SEQ`, XML-escaped
  `LastChange`

Known simplifications:

- Requests are handled sequentially per connection; heavy polling clients can
  starve others.
- `TrackMetaData` is empty (no DIDLLite).
- The protocol info list is static (`SINK_PROTOCOL_INFO` const); it does not
  come from the active player backend.
- UDN is the constant `uuid:mini-mdr`; it is not a persistent per-install UUID,
  so two instances cannot run on one LAN.
- SSDP binds all interfaces and picks a single local IP for `LOCATION`.
- GENA NOTIFY fire-and-forget: no read of the subscriber's HTTP response and no
  spec-mandated resend-until-200 behavior.
- The tray Start/Stop Cast label toggles optimistically; if the app-layer toggle
  fails the label can desync until the next toggle.

Priority protocol work:

1. Serve requests concurrently (thread pool or async runtime) while keeping
   player/state locks short.
2. Persist a stable per-install UDN in the config file.
3. Emit minimal DIDLLite for `TrackMetaData`/`CurrentURIMetaData`.
4. Derive `SinkProtocolInfo` from the active backend (add a trait method).
5. Handle multiple network interfaces for SSDP `LOCATION`.
6. Add integration tests that drive HTTP/SOAP routing end-to-end.

## Configuration

Default configuration:

```toml
[device]
name = "mini-mdr(<hostname>)"   # hostname from $HOSTNAME or $HOST

[player]
backend = "mpv"
mpv_path = "mpv"

[settings]
port = 7878
max_history = 200
```

Device name includes the system hostname by default (e.g., `mini-mdr(laptop)`).
The hostname is read from `$HOSTNAME` or `$HOST` environment variables,
falling back to `localhost`.

History is persisted to `~/.config/mini-mdr/history.json` (or platform
equivalent via `directories::ProjectDirs`). Each entry records timestamp, URI,
and optional title. The `max_history` setting (default 200, max 10000) controls
how many entries are kept. The history file is the single source of truth; the
in-memory `RendererState` no longer caches a `history` field.

Configuration is loaded through `directories::ProjectDirs`. Do not hardcode a
platform-specific config path. Preserve existing user settings when adding
new fields by using serde defaults or an explicit migration strategy.

## Coding Rules

- Prefer small, clear changes over broad rewrites.
- Release builds run under the Windows GUI subsystem
  (`windows_subsystem = "windows"` via cfg_attr in `src/main.rs`), so there is
  no console and stderr writes would fail. Never use `eprintln!`/`println!`;
  use the panic-free `crate::log_error!` / `log_warn!` / `log_info!` macros
  from `src/log.rs`.
- All user-visible text goes through `i18n::t(lang, "message-id")` backed by
  Fluent `.ftl` files under `locales/`. Never hardcode Chinese or English strings
  in UI code. Add new keys to both `locales/en/main.ftl` and
  `locales/zh-CN/main.ftl`.
- Use `anyhow::Result` at application boundaries and preserve actionable error
  context.
- Do not use `unwrap()` or `expect()` in production paths.
- Do not silently discard fallible operations. Log or propagate errors.
- Keep platform-specific behavior behind the relevant abstraction.
- Avoid adding a runtime plugin ABI unless explicitly requested.
- Keep the tray menu limited to the three product actions.
- Use ASCII in source and documentation unless a user-facing localized label
  requires otherwise.
- SSE: `GET /events` pushes `status` / `history` by watching `history.json`
  (via `notify`), not by polling or by diffing an in-memory history cache.
- Adding a UI language: add an entry to `LANGUAGES` in `src/i18n.rs` and a new
  `locales/<code>/main.ftl`; no settings-page code change is required. The
  language `<select>` is populated dynamically from `LANGUAGES`.

## Verification

When a Rust toolchain is available, run:

```text
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

Always run `cargo fmt` before committing. CI enforces `cargo fmt --check` and
`cargo clippy -D warnings`.

Protocol verification should check:

- No settings listener exists immediately after process startup.
- Open Settings starts exactly one loopback server and reuses it.
- Settings requests with a foreign `Origin` or `Sec-Fetch-Site: cross-site`
  are rejected with 403; device names containing control characters are
  rejected on save.
- mpv IPC reads are bounded (`IPC_RESPONSE_TIMEOUT`); a hung mpv must not stall
  the UPnP server beyond that window.
- SSDP `M-SEARCH` returns a reachable `LOCATION` for device, rootdevice, UDN,
  and each of the three service search targets.
- `GET /device.xml` advertises the three expected services.
- `SetAVTransportURI` reaches the selected backend.
- Play and pause return UPnP error 714 when no URI has been loaded; volume,
  mute, stop, and seek reach the backend.
- Stop Cast releases the HTTP and SSDP services.
- Quit terminates mpv and the tray event loop.
- Missing mpv produces an error instead of a panic.

## Do Not Assume

- The existing code compiles; it has not been verified if Rust is unavailable.
- The current HTTP servers are minimal hand-rolled implementations, not
  production-grade HTTP stacks.
- A DMC will accept every simplified XML response (especially empty
  `TrackMetaData`).
- `mpv` is installed on the target machine; without it, Cast still starts and
  playback actions return UPnP 501 faults.
