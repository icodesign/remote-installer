<div align="center">

# Remote Installer

**Install signed iOS and Android builds on real devices over the internet**

Perfect for AI Agents and vibe coding remotely.

No TestFlight or Google Play Beta. No hosted storage. No waiting for processing.

`Remote Desktop → Link / QR code → Phone Install`

</div>

```bash
remote-installer share ./MyAwesome.ipa
```

Remote Installer validates the build, opens a temporary HTTPS link, and prints a QR code. Scan it with the phone camera or open the page in the phone's browser to install.

> iOS: Works with development and ad hoc builds. The target iPhone must already be included in the provisioning profile.
> Android: Works with a signed standalone APK. Android App Bundles (`.aab`) and `.apks` sets are not directly installable and are intentionally rejected. Split-only APKs are rejected when `apkanalyzer` is available.

## Quick start

### 1. Install

```bash
# Install cloudflared for quick tunnels
# It does NOT require Cloudflare account.
brew install cloudflared
# Install remote-installer
# with homebrew
brew install icodesign/tap/remote-installer
# or npm
npm install --global @icodesign/remote-installer
```

For a one-off run without installing a command globally, use the npm package:

```bash
npx --yes @icodesign/remote-installer share /path/to/MyApp.ipa
```

The Homebrew and npm distributions currently support macOS arm64 (Apple silicon) and macOS x86_64 (Intel). Other host operating systems are not yet included in the release artifacts.

### 2. Share a build

Share an IPA:

```bash
remote-installer share /path/to/MyApp.ipa
```

Or share a signed `.app` built for a real device:

```bash
remote-installer share [PATH_TO_APP_BUILD_IPA_OR_APK]
```

### 3. Install on the phone

1. Keep the command running.
2. Open the link in the phone browser or scan the QR code with the camera.
3. Tap **Install**.

## What you get

- A temporary HTTPS install page and QR code
- Support for `.ipa`, signed device `.app`, and signed standalone `.apk` builds
- Structural validation before the build is exposed, with deeper Android metadata and signature checks when Android SDK tools are available
- Live download progress in the terminal
- Automatic cleanup when sharing ends
- Optional expiry and download limits
- Cloudflare Quick Tunnel by default, or Tailscale Funnel for sensitive builds

Remote Installer distributes an existing build. It does **not** sign, re-sign,
register devices, convert App Bundles or split APKs, or make Simulator and App
Store builds installable.

## For AI agents

This repository includes a skill for coding agents:

```bash
npx skills add icodesign/remote-installer
```

Example agent request:

> Create a new build with latest changes for my iPhone, and give me the install URL with remote-installer.

## Common recipes

### One person, one hour

```bash
remote-installer share MyApp.ipa \
  --max-downloads 1 \
  --expire-after 1h
```

The command exits when either limit is reached, after allowing an active download to finish.

### Useful options

| Option               | Purpose                                |
| -------------------- | -------------------------------------- |
| `--expire-after 30m` | Stop sharing after a duration          |
| `--timeout 300`      | Stop sharing after a number of seconds |
| `--max-downloads 3`  | Stop after a number of downloads       |
| `--no-qr`            | Do not print the terminal QR code      |

Use either `--expire-after` or `--timeout`, not both. Run `remote-installer share --help` for every option.

## Important security notes

- **The link is the credential.** Anyone who receives it can install the build, and it can be forwarded.
- Use `--expire-after` and `--max-downloads` when sharing with someone else.
- The build remains on your Mac rather than being uploaded for storage.
- With Cloudflare, TLS terminates at Cloudflare while the build is transferred.
- Stopping the command closes the tunnel and deletes Remote Installer's temporary copy.

## Troubleshooting

| Problem                                   | What to check                                                                                                           |
| ----------------------------------------- | ----------------------------------------------------------------------------------------------------------------------- |
| **“Unable to Install”**                   | The iPhone UDID is in the embedded profile, the profile is valid, and the app was rebuilt after registering the device. |
| **Install button does nothing**           | Open the install page in Safari. Some third-party iOS browsers do not hand the install link to the system.              |
| **`cloudflared CLI was not found`**       | Run `brew install cloudflared`, or pass `--cloudflared-bin /path/to/cloudflared`.                                       |
| **`app is not an iphoneos device build`** | Use `Build/Products/Debug-iphoneos/`, not `Debug-iphonesimulator/`.                                                     |
| **Provisioning profile expired**          | Refresh signing in Xcode and rebuild.                                                                                   |
| **Warning: `apkanalyzer was not found`**  | Sharing continues without manifest metadata or split-APK validation. Install Android SDK Command-Line Tools for full checks. |
| **Warning: `apksigner was not found`**    | Sharing continues without signature verification. Install Android SDK Build Tools for full checks.                    |
| **APK signature verification fails**      | Produce a signed debug or release APK; Remote Installer does not sign it.                                               |
| **Android asks to allow this source**     | Allow APK installation for the browser that opened the link, then open the download again.                              |
| **Android update is rejected**            | The APK must use the same signing certificate as the installed app and an acceptable version code.                      |

Most installation failures are signing or provisioning problems. Remote Installer can validate and distribute a build, but it cannot make an incorrectly signed build installable.

For full APK validation, install the Android SDK `apkanalyzer` and `apksigner`
tools. They are discovered from `PATH`, `ANDROID_SDK_ROOT`, `ANDROID_HOME`, and
standard SDK locations. Use `--apkanalyzer-bin` and `--apksigner-bin` when the
SDK is installed elsewhere. If either tool cannot be discovered, sharing still
starts after structural checks and prints a warning describing the skipped
validation.
