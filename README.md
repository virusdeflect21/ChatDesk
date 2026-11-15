# ChatDesk

**A tiny Windows shell around [Google Chat](https://chat.google.com) — an app window, not a browser.**

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-Windows%2010%20%2F%2011-0078d4)](#requirements)
[![Built with Rust](https://img.shields.io/badge/built%20with-Rust%20%C2%B7%20tao%20%C2%B7%20wry-b7410e)](https://www.rust-lang.org)

ChatDesk wraps the official Google Chat web client in a native window built with Rust + [tao](https://docs.rs/tao) + [wry](https://docs.rs/wry) (WebView2). There are no tabs, no address bar, no sync and no extensions — just chat, a tray icon and a deliberately trimmed-down web engine.

No setup required: no Google Cloud project, no OAuth client, no API keys. Whatever Google account you sign into inside the window is the account it uses — including consumer `@gmail.com` accounts, because it loads exactly the same web client your browser would.

> Unofficial project. Not affiliated with, endorsed by or supported by Google.

## Features

- **~0.7 MB executable** — the browser engine is the WebView2 runtime Windows already has, so nothing is bundled.
- **Locked to Google Chat** — no new windows, no tab spawning, no wandering off to other Google sites.
- **Lives in the tray** — closing the window keeps messages arriving; a red badge and tray balloons tell you what's new.
- **Low idle memory** — the page is pushed to the standby list and the engine's working set is handed back to Windows when hidden.
- **Isolated profile** — its own session under `%LOCALAPPDATA%`, separate from your browser, with capped caches.
- **Single instance** — launching it again focuses the window you already have.
- **Fast first paint** — an inline splash frame appears in roughly half a second while the real client loads behind it.

## Requirements

- Windows 10 or 11
- [Evergreen WebView2 Runtime](https://developer.microsoft.com/microsoft-edge/webview2/) — preinstalled on up-to-date systems

## Install

### Download

Grab `ChatDesk-v0.1.0-windows-x64.exe` from the [latest release](https://github.com/virusdeflect21/ChatDesk/releases/latest). No installer, no runtime to fetch — just run it. The build is unsigned, so SmartScreen may warn on first launch.

### From source

```powershell
git clone https://github.com/virusdeflect21/ChatDesk.git
cd ChatDesk
cargo build --release
.\target\release\chatdesk-web.exe
```

Or just `cargo run --release`.

Sign in with the Google account you want to use. After the first successful load the window is pinned to `chat.google.com`; only `chat.google.com` and `accounts.google.com` may navigate inside it. External `https://` links shared in chat open in your real browser instead.

If you ever get stuck, delete `%LOCALAPPDATA%\ChatDeskWeb\profile` to reset the session.

## Configuration

All knobs are environment variables read at launch:

| Variable | Effect |
| --- | --- |
| `CHATDESK_WEB_DEEP_SLEEP=1` | Shut the engine down entirely after `CHATDESK_WEB_TEARDOWN_SECS` in the tray. ~3 MB idle, but no messages until you reopen (chat is back in ~4 s). |
| `CHATDESK_WEB_TEARDOWN_SECS=<n>` | Seconds in the tray before deep sleep triggers. Default `30`. |
| `CHATDESK_WEB_SOFTWARE_RENDERING=1` | No GPU context — about 15 MB less while visible, but less smooth scrolling. |
| `CHATDESK_WEB_FULL_JIT=1` | Full V8 JIT for faster JS at higher RAM. |
| `CHATDESK_WEB_WINDOW_MARKER=<path>` | Write the first-frame timestamp (ms after process start) to a file. |
| `CHATDESK_WEB_LOADED_MARKER=<path>` | Write the "chat finished loading" timestamp to a file. |

## Size and RAM: the honest numbers

The **application itself is ~0.7 MB** (one Rust exe). It ships **no browser engine**: WebView2 is the shared Evergreen runtime Windows/Edge already installed, so the engine adds no disk footprint of its own. The isolated profile (`%LOCALAPPDATA%\ChatDeskWeb\profile`) is capped caches plus your session data; it was ~33 MB after real use.

There is no smaller engine choice that can run `chat.google.com`: Google's client needs a full modern Chromium-grade engine, and WebView2 is the only one already present on Windows (bundling CEF would add ~200 MB of disk instead of using the OS one).

RAM, measured with `chat.google.com` loaded (Windows 11, WebView2 157). "Task Manager" is private working set summed over the whole process tree; run [`check_mem.ps1`](check_mem.ps1) to see it yourself:

| State | Task Manager | Processes |
| --- | --- | --- |
| Visible, focused, chat loaded | ~230–250 MB | 5 |
| Minimized / unfocused for 45 s (caches trimmed) | ~80–100 MB | 5 |
| Closed to tray, **messages still arrive** (default) | ~25–30 MB | 5 |
| Closed to tray with `CHATDESK_WEB_DEEP_SLEEP=1` (engine off) | ~3 MB | 1 |

How it gets there:

- **One renderer, nothing spare**: `--renderer-process-limit=1`, site isolation off and no spare renderer (Chat's cross-origin frames otherwise each get their own ~80 MB renderer).
- **Fewer processes**: GPU and network services run inside the browser process.
- **V8 lite mode** (interpreter only, `--optimize-for-size`), worth about 20–50 MB in the renderer. Set `CHATDESK_WEB_FULL_JIT=1` to turn the optimizing JIT back on.
- Background networking, component updates, SmartScreen, translate, optimization hints, media router and similar services are off. Background throttling stays on (minus the 1-minute "intensive" throttling, so tray messages aren't delayed).
- **Idle trim**: after 45 s without focus the engine is set to [`MemoryUsageLevel::Low`](https://learn.microsoft.com/en-us/dotnet/api/microsoft.web.webview2.core.corewebview2memoryusagetargetlevel) and the working set of the whole WebView2 tree is emptied.
- **Passive tray mode**: the page keeps running (about 1% of one core) so new messages still raise tray balloons and the badge. The controller is hidden, and the tree's working set is handed back to Windows immediately and every 60 s. Those pages sit on the standby list or in the pagefile, so the RAM really is free for other apps; commit stays around 280 MB, and anything the page touches is faulted back in on demand.

The floor while visible is Google's page itself. Chat's JS heap is about 80 MB live, plus DOM, layout and images, so ~230 MB is close to the minimum for a working `chat.google.com`.

## Locked to Google Chat

After `chat.google.com` loads once, the window is pinned to it:

- **All new-window/tab requests are denied** — Ctrl+T/Ctrl+N-style pop-ups cannot exist (WebView2 browser accelerator keys are disabled, and every `window.open` is refused).
- **Only `chat.google.com` and `accounts.google.com` may navigate in the window.** Clicking your avatar, the Gmail/Drive links, or anything else that tries to pull you to another Google property is refused — the window stays on chat. External (`https://`) links shared in chat open in your real browser instead.
- Before the first successful chat load, `accounts.google.com` sign-in popups are allowed so authentication works.

Sign-out still works (it goes through the auth host).

## Notifications

- Balloons come from ChatDesk's own tray icon. On startup ChatDesk registers its app identity (`HKCU\Software\Classes\AppUserModelId\ChatDesk.ChatDeskWeb`, name **ChatDesk (Google Chat)** plus icon), so Windows labels them correctly instead of guessing.
- Google Chat re-raises notifications for already-pending messages right after it loads. Anything in the first 20 s after a load is not ballooned; the unread badge still shows.

## Fast launch

A precomputed base64 `data:` URL splash paints the first frame in ~550–750 ms from process start; the real client URL is loaded only after the shell is visible (window handle appears in ~180–330 ms).

To measure it yourself, set both marker variables and read the files after launch (values are milliseconds after process start):

```powershell
$env:CHATDESK_WEB_WINDOW_MARKER="$env:TEMP\w.txt"   # first frame the window is visible
$env:CHATDESK_WEB_LOADED_MARKER="$env:TEMP\l.txt"   # chat.google.com finished loading
Start-Process "target\release\chatdesk-web.exe"
Get-Content "$env:TEMP\w.txt", "$env:TEMP\l.txt"
```

## Other behavior

- Downloads land in your normal `Downloads` folder.
- The user agent presents as Edge because Google's account pages are sensitive to embedded-webview UAs.
- Dark theme is forced on the webview.

## Project layout

```
.
├── build.rs          # generates the app, badge and overlay icons at build time
├── src/main.rs       # window, WebView2 tuning, tray, notifications, single instance
├── check_mem.ps1     # prints per-process RAM for the ChatDesk tree
└── Cargo.toml
```

## License

[MIT](LICENSE) © 2026 virusdeflect21
