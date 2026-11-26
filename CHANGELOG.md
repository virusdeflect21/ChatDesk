# Changelog

## v0.2.2

**Fixes**
- **Reset App Data could fail to restart.** The new instance could start before the old one released the single-instance lock, so it would just poke the dying process and exit. The single-instance listener is now stopped and joined before the app respawns.
- **Reset could leave profile files behind.** The WebView2 browser process needs a moment to release the profile after teardown; ChatDesk now waits (up to 5 s) for it to exit before wiping the directory.

**New**
- **Unread counter on the Windows tray icon:** the red badge now shows the actual unread count (1–99) drawn into the icon itself, messenger-style. Tooltip shows the exact number. Linux/macOS keep the plain red-dot badge.

**Quality of life**
- Your **login is remembered across updates and reinstalls**: the session lives in the app's data directory (`%LOCALAPPDATA%\ChatDeskWeb` on Windows), not with the executable. Installing a new ChatDesk build — or putting the exe somewhere else — keeps you signed in.
- Fresh README with banner, per-OS install guides and troubleshooting; new in-repo logo/banner art.

**Behind the scenes**
- Verified there are no leak paths in the shell itself: the event loop holds no per-message allocations, tray/notification state is bounded, and working-set trims run on the whole WebView2 process tree.

## v0.2.1

**New**
- **Linux and macOS builds.** The same app now ships for Windows, Linux (webkit2gtk) and macOS (WKWebView). Autoboot works everywhere: `HKCU\...\Run` key (Windows), `~/.config/autostart` entry (Linux), LaunchAgent (macOS).
- **Links open in your browser on a plain click** — Docs, Drive, Meet, Calendar, anything shared in chat. Previously, google.com-hosted targets were silently swallowed instead of opening.
- **Even less RAM:** V8 heap cap 128→96 MB, disk cache 16→8 MB, media cache 4→2 MB, idle cache trims 20→15 s, hidden trims 30→20 s.
- **Off-screen window guard:** if a monitor was unplugged, the saved window position is ignored instead of restoring off-screen.

## v0.2.0

**New**
- **Start with Windows (autoboot)** switch in the tray menu, plus **Start hidden in tray** — both remembered.
- **Window memory:** size, position and maximized state persist between launches.
- **Zoom** (in/out/reset, persisted), **Back / Forward / Reload / Back to Chat**, **Check for updates**, **Reset app data** — all in the tray menu.
- **~50% lower RAM and CPU than v0.1.0:** intensive wake-up throttling enabled, V8 heap cap 256→128 MB, caches halved, idle trims 45→20 s, hidden trims 60→30 s.

## v0.1.0

- Initial release: ~0.7 MB shell around chat.google.com with tray icon, unread badge, balloon notifications, isolated profile, single-instance lock, deep-sleep mode.
