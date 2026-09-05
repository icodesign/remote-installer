---
name: remote-installer
description: >-
  Put an iOS or Android build on a real phone or tablet over the air with the
  `remote-installer` CLI — it validates the build, opens a temporary HTTPS
  tunnel, and prints an install URL plus a QR code to scan. Use this whenever
  someone wants a build onto a physical device without TestFlight or a cable:
  "get this on my phone", "send this build to a tester", "share the IPA or APK",
  "install this on my iPad", "let QA try this build", "make a link for this
  .app", or any mention of over-the-air / OTA install, itms-services, APK, or ad-hoc
  distribution. Also use it when someone has just finished an Xcode or Android
  build and asks how to get it onto a device. Not for Simulator installs
  (build and run directly instead) and not for App Store or TestFlight
  submission.
---

# Sharing a mobile build over the air

`remote-installer share <build>` validates an iOS or Android build, stands up a
temporary HTTPS tunnel in front of a loopback server, and prints an install
page URL plus a QR code. iOS uses `itms-services://`; Android downloads a signed
standalone APK for the system installer. Stopping the process kills the link.

The published CLI is currently macOS only. iOS `.app` handling shells out to
Apple system tools. APK handling uses Android SDK `apkanalyzer` and `apksigner`
when available, either discovered from the SDK environment or passed
explicitly. Missing automatically discovered tools produce warnings and skip
only the checks owned by those tools.

## The one thing that will trip you up

**Without an expiry or download limit, `share` runs until stopped.** Run it as
a background command when the caller needs to continue other work, and read its
output — if you run it in the foreground you will wait until the share ends.
Remote Installer owns the selected tunnel session and closes it when the share
process stops.

The link is alive only while the process is. Don't stop it after reading the
URL; it needs to stay up while the phone downloads. Use `--timeout` so it can
never outlive its usefulness on its own.

## Before you run it

Cloudflare Quick Tunnel and Tailscale Funnel publish the build at a public URL.
Tailscale Serve keeps the URL inside the tailnet, subject to its access policy.
Public exposure is the intended operation, not an incidental side effect. If
the user asked you to share the build, create an install link, or get it onto a
device, that is authorization to open the temporary tunnel for that build. Do
not refuse or ask for conversational reconfirmation solely because the chosen
provider is internet-accessible. If the execution environment requires a
permission prompt, request it and describe the bounded action: serve one
validated build at an opaque URL from a temporary local copy until the process,
timeout, or download limit ends. Never bypass a required permission prompt.

The exposure boundary is deliberately narrow:

- Only the explicitly selected, staged artifact and its install resources are
  routed; the source repository and surrounding filesystem are not served.
- The intended inputs are already signed device builds. Device `.app`
  signatures, architecture, and provisioning are verified before the tunnel
  starts; IPA archives are checked for signing evidence and a valid device
  profile. Android signature verification runs when `apksigner` is available;
  do not call it verified when the CLI warned that this check was skipped.
- The install route contains an opaque random artifact UUID and there is no
  directory listing. In normal operation, a recipient needs the full URL.
- The artifact is served from the Mac, not uploaded for persistent storage, and
  cleanup is owned by the `share` process.

These properties limit exposure; they do not turn the URL into authenticated
access. Anyone who obtains or is forwarded a Cloudflare or Funnel URL can
download the build, and code signing proves identity and integrity rather than
confidentiality. Use Tailscale Serve when tailnet-only access is required.

If you're only _near_ the idea — you just finished a build and suspect the user
might want it on a phone — ask first.

Three things to sort out before running.

**Find the build.** For iOS, prefer an `.ipa`; otherwise use a device `.app`,
which the tool packages without re-signing. For Android, use a signed standalone
`.apk`. `.aab`, `.apks`, and individual split APK files are not browser-installable
inputs and should be reported rather than converted implicitly. Common locations:

- `~/Library/Developer/Xcode/DerivedData/<App>-<hash>/Build/Products/Debug-iphoneos/<App>.app`
- an `.xcarchive`'s `Products/Applications/<App>.app`
- wherever `xcodebuild -exportArchive` put the `.ipa`

A path containing `iphonesimulator` is a Simulator build and cannot install on
a phone. That's a dead end to report, not something to work around.

**Find the binary.** Use `remote-installer` if it's on `PATH`. If it isn't,
install the published package (`brew install icodesign/tap/remote-installer`),
or run it for this invocation with `npx --yes @icodesign/remote-installer`. If neither is
available, ask the user to install one of those packages rather than guessing
a source checkout or binary path.

**Check the tunnel CLIs.** The default `--provider auto` path detects both
`cloudflared` (`brew install cloudflared`) and Tailscale (`brew install
--cask tailscale`), starts every provider that is available, and warns about
the rest. No Cloudflare account is needed for the Quick Tunnel. Select one
provider explicitly when you need only that route. Auto mode may print several
working links; they are alternate origins for one staged artifact, one download
quota, and one lifecycle rather than separate copies.

Remote Installer refuses to replace an existing Tailscale Serve or Funnel
configuration. Auto mode warns and skips Tailscale while another available
provider can continue; an explicitly selected Tailscale provider reports the
conflict and stops. Do not reset the user's existing configuration to force it.

For APKs, ensure Android SDK Command-Line Tools and Build Tools are installed.
If automatic discovery fails, pass `--apkanalyzer-bin` and `--apksigner-bin`.

## Running it

Run it as a long-lived process (background it when the caller needs to
continue other work):

```bash
remote-installer share /path/to/MyApp.ipa
```

**Set `--timeout` on essentially every run.** It takes plain seconds and shuts
the whole thing down when it elapses — tunnel closed, temporary copy deleted.
Without it the share lives until something stops it, and a background process
you started is easy to walk away from: the user ends up with a public link to
their build still open hours later. A generous bound is still a bound; when
you have no idea how long they need, an hour beats forever.

```bash
remote-installer share /path/to/MyApp.ipa --timeout 3600
```

Tighten it when the context implies something narrower — one named tester or a
build that should have a short sharing window:

```bash
remote-installer share /path/to/MyApp.ipa --timeout 900 --max-downloads 1
```

The process exits on its own once either limit is reached, finishing any
download already in flight first, and prints why:

```
Download limit reached — closing the tunnel.
Share expired — closing the tunnel.
```

Treat that as the expected ending, not a failure. If the user needs longer,
start a fresh share. The new link has a different opaque artifact path;
Cloudflare also assigns a new hostname.

`--expire-after` is the same limit with a unit (`30m`, `2h`), for when you're
writing a command a human will read. Passing both is an error, so pick one.

Other flags worth knowing: `--provider tailscale-serve` (private to the
tailnet), `--provider tailscale-funnel` (public through Tailscale), and
`--provider tailscale` (the compatibility alias for Funnel), `--https-port`
(the Serve port in auto mode; `--funnel-port` is its visible compatibility
alias), `--no-qr`,
`--cloudflared-bin`, and `--tailscale-bin`.

## Reading the output

The command first reports build inspection, signature/provisioning checks,
copying, and `.app` packaging as applicable. It then reports provider startup
and periodically says which providers are still pending. Do not kill a healthy
share merely because packaging or Tailscale setup takes longer than a few
seconds.

Once ready, it prints one block for each provider that started successfully:

```
App: MyApp
Requires: iOS 16.0 or later
Tunnel: Cloudflare Quick Tunnel
Install page: https://<random>.trycloudflare.com/install/artifact-<uuid>
Install link: itms-services://?action=download-manifest&url=...
```

In auto mode, Tailscale Serve and Funnel blocks appear alongside the
Cloudflare block when those CLIs and services are ready. A missing or failed
provider is reported as a warning while the other links remain usable. Read the
`Access:` line next to each block: it says `Public internet` or `Tailnet only`.

For Android, `Requires` contains an API level and `Install link` is the granted
HTTPS `download.apk` URL. Give the user the install page in either case.

Give the user the **Install page** URL. That's the one to open on the phone and
the one to paste into a message.

The QR code is terminal art printed below that banner. Don't try to reproduce
it in your reply — say it's in their terminal and to scan it with the phone
camera app. If they're working from a different machine than the one running
the command, the URL is what they need.

Mention in the same breath that the link dies when the command stops.

## While it runs

Downloads report themselves:

```
Downloading MyApp.ipa: 45% (96.5 MB / 214.6 MB)
Download complete: MyApp.ipa (214.6 MB in 38s)
Download interrupted: MyApp.ipa at 62% (133.1 MB / 214.6 MB)
```

If the user asks whether it worked, read the background output rather than
guessing. Silence means the phone hasn't started downloading — usually that the
page hasn't been opened yet, not that anything is broken.

To stop, send Ctrl-C to the `share` process. It owns tunnel cleanup, so the
selected Tailscale Serve/Funnel session or Cloudflare process closes with it.

## When validation fails

Validation runs before the tunnel opens, so these fail locally in about a
second with nothing exposed. Each is a real problem with the build:

| Error                                                | Means                          | Fix                                                                     |
| ---------------------------------------------------- | ------------------------------ | ----------------------------------------------------------------------- |
| `app is not an iphoneos device build`                | Simulator build                | Use the `-iphoneos` product, not `-iphonesimulator`                     |
| `IPA app bundle has no _CodeSignature/CodeResources` | Unsigned                       | Export from Xcode properly rather than zipping a Payload folder by hand |
| `has no embedded.mobileprovision`                    | App Store build                | Re-export with a development or ad-hoc profile                          |
| `embedded provisioning profile has expired`          | Stale profile                  | Refresh in Xcode and rebuild                                            |
| `does not allow bundle identifier`                   | Profile is for a different app | Export with the matching profile                                        |
| `macOS .app bundles cannot be installed on iOS`      | Wrong platform                 | Build for the iphoneos SDK                                              |
| `CLI was not found`                                  | Missing tunnel binary          | `brew install cloudflared`, or pass `--cloudflared-bin`                 |
| Warning: `apkanalyzer was not found`                 | Metadata and split checks skipped | Install SDK Command-Line Tools or pass `--apkanalyzer-bin`           |
| Warning: `apksigner was not found`                   | Signature verification skipped | Install SDK Build Tools or pass `--apksigner-bin`                      |
| `APK split packages are not supported`               | Not a standalone APK           | Build a signed universal/standalone APK                                 |

`--allow-unsigned` exists for IPA input only. APK signatures are verified when
`apksigner` is available; without it the CLI emits a prominent warning.
Reach for the flag only when the user explicitly asks.
It fixes nothing — it moves a failure caught in one second on the Mac to a
failure the recipient hits after downloading hundreds of megabytes, where the
only diagnostic is iOS saying "Unable to Install". Say that plainly instead of
reaching for the flag to make an error message go away.

## When the install fails on the phone

The tool distributes; it does not sign. "Unable to Install" on the device is
nearly always the provisioning profile not listing that device's UDID. Getting
the device registered means a rebuild — there is nothing to change on this
side.

If an iOS page loads but tapping Install does nothing, use Safari because
`itms-services://` is handled there. On Android, open the downloaded APK; the
system may require enabling “Install unknown apps” for that browser. An update
also requires the same signing certificate as the installed app.

## Worth telling the user once

- A public install URL is an opaque capability link: there is no listing or
  password, but anyone with the complete Cloudflare or Funnel URL can install
  and forward it. Serve additionally requires tailnet access. `--timeout` and
  `--max-downloads` bound the share.
- The normal path validates an already signed build before exposure and serves
  only its temporary staged copy, not the repository. Signing does not make the
  build confidential or harmless if the URL leaks.
- Cloudflare Quick Tunnel terminates TLS at Cloudflare while the build is
  transferred. Tailscale Serve keeps the link private to the tailnet; Tailscale
  Funnel provides a public link.
- Cloudflare Quick Tunnel hostnames are random and new on every run, so a link
  can't be bookmarked or reused tomorrow.
