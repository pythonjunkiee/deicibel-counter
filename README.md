# dB Meter — Gaming Overlay

A lightweight, transparent microphone decibel counter that sits on top of your game. Built with Tauri 2 (Rust) + React.

---

## Download & Install

### Windows

1. Go to the [Releases page](https://github.com/pythonjunkiee/deicibel-counter/releases/latest)
2. Under **Assets**, click the file ending in `_x64-setup.exe`
3. Run the downloaded file
4. If Windows SmartScreen appears:
   - Click **"More info"**
   - Click **"Run anyway"**
5. Done — find **dB Meter** in your Start Menu

### macOS

1. Go to the [Releases page](https://github.com/pythonjunkiee/deicibel-counter/releases/latest)
2. Under **Assets**, click the `.dmg` file
3. Open the `.dmg` and drag **dB Meter** into your Applications folder
4. First launch: right-click the app → **Open** → **Open** (one-time Gatekeeper bypass)

### Android

1. Go to the [Releases page](https://github.com/pythonjunkiee/deicibel-counter/releases/latest)
2. Under **Assets**, click the `.apk` file — download it to your phone
3. Tap the downloaded file
4. If prompted: go to **Settings → Install unknown apps** and allow it
5. Tap **Install**

> **iOS** is not supported at this time (Apple requires a paid developer account for all iOS installs).

---

## How to Trigger a New Release

Push a version tag and GitHub Actions builds everything automatically (~15 min):

```bash
git tag v1.0.0
git push origin v1.0.0
```

The release will appear at `https://github.com/pythonjunkiee/deicibel-counter/releases`

---

## Features

- Live dB level from your microphone, updating 10×/second
- Color bar: green (safe) → amber (approaching limit) → red (over threshold)
- Compact 80×80 micro mode — sits in the corner while gaming
- Auto-calibration: speak for 10 seconds, threshold sets itself
- Device selector — switch mics without restarting
- Mute toggle — freezes the display, mic stream stays open for instant resume
- Transparent, always-on-top, draggable overlay

---

## Tech Stack

| Layer | Technology |
|---|---|
| Audio capture | Rust — `cpal` (WASAPI / CoreAudio / AAudio) |
| App shell | Tauri 2 |
| UI | React 18 + TypeScript + Tailwind CSS |
| CI/CD | GitHub Actions |
