# Tray menu
tray-cast = Cast
tray-more = More
tray-quit = Quit

# Settings page
settings-title = mini-mdr Settings
settings-subtitle = Local DMR settings and runtime status
settings-section-settings = Settings
settings-device-name = Device Name
settings-player-backend = Player Backend
settings-mpv-path = mpv Executable Path
settings-vlc-path = VLC Executable Path
settings-save = Save Settings
settings-language = Language
settings-language-system = System default
settings-saved = Settings saved. Language takes effect immediately. Other settings require a restart of Cast.
settings-section-history = History
settings-history-index = #
settings-history-time = Time
settings-history-title = Title
settings-history-copy-link = Copy Link
settings-history-open-in-browser = Open in Browser
settings-history-copied = Link copied
settings-history-empty = No history yet
settings-history-empty-sub = Your played media history will appear here.
settings-history-subtitle = View your recently played media history.
settings-max-history = Max History Entries
settings-status = Status
settings-hint = Device name, player, and max history settings require a restart of Cast (stop then start) to take effect.

# Tabs
tab-settings = Settings
tab-history = History
tab-about = About
tab-guide = Guide

# About page
about-title = About
about-subtitle = Learn more about mini-mdr.
about-description = mini-mdr is a tray-only UPnP/DLNA MediaRenderer. It advertises a DMR on your LAN and plays media through mpv or VLC.

# Guide page
guide-title = Guide
guide-intro = mini-mdr is a tray-only UPnP/DLNA MediaRenderer. It does not decode audio or video itself — it relies on an external player backend.
guide-player-heading = 1. Install a player backend
guide-player-text = Install mpv or VLC on your system, then open Settings and select the player backend and set its executable path. mini-mdr launches the player to play media pushed by a DLNA/UPnP controller (for example a phone app).
guide-config-heading = 2. Where data is stored
guide-config-text = Your configuration and play history are stored at:
guide-usage-heading = 3. How to use
guide-usage-text = After launch, mini-mdr advertises a DMR on your local network. Use any DLNA/UPnP controller to cast to "mini-mdr". Toggle Cast from the tray menu, and change the device name, language, and player in Settings.

# Validation errors
error-save-failed = Save failed
error-name-length = Device name must be 1 to 128 characters
error-name-control = Device name must not contain control characters
error-unsupported-backend = Unsupported player backend
error-player-path-empty = Player executable path must not be empty
error-language-invalid = Unknown language selection

# State display
cast-stopped = Stopped
cast-running = Running
transport-no-media = No Media
transport-stopped = Stopped
transport-playing = Playing
transport-paused = Paused
transport-loading = Loading
