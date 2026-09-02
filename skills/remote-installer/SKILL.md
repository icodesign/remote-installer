---
name: remote-installer
description: >-
  Put an iOS build on a real iPhone or iPad over the air with the
  `remote-installer` CLI — it validates the build, opens a temporary HTTPS
  tunnel, and prints an install URL plus a QR code to scan. Use this whenever
  someone wants a build onto a physical device without TestFlight or a cable:
  "get this on my phone", "send this build to a tester", "share the IPA",
  "install this on my iPad", "let QA try this build", "make a link for this
  .app", or any mention of over-the-air / OTA install, itms-services, or ad-hoc
  distribution. Also use it when someone has just finished an Xcode build or
  archive and asks how to get it onto a device. Not for Simulator installs
  (build and run directly instead) and not for App Store or TestFlight
  submission.
---

# Sharing an iOS build over the air

`remote-installer share <build>` validates an iOS build, stands up a temporary
HTTPS tunnel in front of a loopback server, and prints an install page URL, an
`itms-services://` link, and a QR code. Someone scans it, taps Install, and the
app lands on their home screen. Stopping the process kills the link.

macOS only — the tool shells out to `codesign`, `lipo`, `security`, `ditto`
and `sips`.

## The one thing that will trip you up

**`share` runs until stopped. It never returns on its own.** Run it as a
background command and read its output — if you run it in the foreground you
will hang until your tool times out, and the tunnel dies with that timeout, so
the user ends up with nothing.

The link is alive only while the process is. Don't stop it after reading the
URL; it needs to stay up while the phone downloads. Use `--timeout` so it can
never outlive its usefulness on its own.

## Before you run it

Opening a tunnel publishes the build to a public URL, so treat it like any
other outward-facing action. If the user asked you to share it, build a link,
or get something onto a device, that's your go-ahead. If you're only _near_ the
idea — you just finished a build and suspect they might want it on a phone —
ask first.

Three things to sort out before running.

**Find the build.** Prefer an `.ipa`. Otherwise a device `.app`, which the tool
packages for you without re-signing. Common locations:

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

**Check `cloudflared` is installed** (`brew install cloudflared`) unless the
user wants `--provider tailscale`. No Cloudflare account is needed.

## Running it

Start it in the background:

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

Tighten it when the context implies something narrower — one named tester, a
build that shouldn't circulate, anything the user calls sensitive or
unreleased:

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
start a fresh share — the new link will have a different hostname.

`--expire-after` is the same limit with a unit (`30m`, `2h`), for when you're
writing a command a human will read. Passing both is an error, so pick one.

Other flags worth knowing: `--provider tailscale` (TLS terminates on the user's
own machine rather than at Cloudflare — worth suggesting for a sensitive build,
though it needs a Tailscale account with Funnel enabled), `--no-qr`,
`--cloudflared-bin`, `--tailscale-bin`.

## Reading the output

Within a few seconds:

```
App: MyApp
Requires: iOS 16.0 or later
Tunnel: Cloudflare Quick Tunnel
Install page: https://<random>.trycloudflare.com/install/artifact-<uuid>
Install link: itms-services://?action=download-manifest&url=...
```

Give the user the **Install page** URL. That's the one to open on the phone and
the one to paste into a message.

The QR code is terminal art printed below that banner. Don't try to reproduce
it in your reply — say it's in their terminal and to scan it with the iPhone
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

To stop, terminate the background process; that's what Ctrl-C does for a human.

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

`--allow-unsigned` exists, but reach for it only when the user explicitly asks.
It fixes nothing — it moves a failure caught in one second on the Mac to a
failure the recipient hits after downloading hundreds of megabytes, where the
only diagnostic is iOS saying "Unable to Install". Say that plainly instead of
reaching for the flag to make an error message go away.

## When the install fails on the phone

The tool distributes; it does not sign. "Unable to Install" on the device is
nearly always the provisioning profile not listing that device's UDID. Getting
the device registered means a rebuild — there is nothing to change on this
side.

If the page loads but tapping Install does nothing, they're probably in a
third-party browser. `itms-services://` needs Safari.

## Worth telling the user once

- Anyone with the link can install; there is no password. `--timeout` and
  `--max-downloads` are how you bound that.
- With the default Cloudflare tunnel the build passes through Cloudflare
  unencrypted, because TLS terminates there. `--provider tailscale` avoids it.
- Cloudflare Quick Tunnel hostnames are random and new on every run, so a link
  can't be bookmarked or reused tomorrow.
