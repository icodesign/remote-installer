# remote-installer

Run the macOS `remote-installer` CLI without installing a Rust toolchain:

```bash
npx --yes @icodesign/remote-installer share /path/to/MyApp.ipa
# or a signed standalone Android APK
npx --yes @icodesign/remote-installer share /path/to/MyApp.apk
```

By default, the command detects the Tailscale and cloudflared CLIs and starts
every provider available on this Mac in parallel. It warns about providers that
are unavailable or not ready. To keep the install page private to the tailnet,
select Tailscale Serve explicitly:

```bash
npx --yes @icodesign/remote-installer share /path/to/MyApp.ipa \
  --provider tailscale-serve
```

Use `--provider tailscale-funnel` for a public Tailscale link. The older
`--provider tailscale` spelling remains an alias for Funnel. Install the
Tailscale app separately; `--tailscale-bin` can point to a non-standard CLI
location. `--https-port` selects the Tailscale HTTPS port, and
`--funnel-port` remains a visible compatibility alias.

Cloudflare and Funnel expose only the selected staged build at an opaque
artifact URL; they do not expose the source tree or a browsable directory. The
build stays on the Mac and disappears from the share when the process stops.
Anyone who learns the full public URL can still forward or download it, so use
`--expire-after` and `--max-downloads` when a bounded share is appropriate.

The package carries arm64 and x86_64 macOS binaries and selects the matching
one at startup. It supports signed iOS device builds and signed standalone
Android APKs; it is not a Simulator installer and does not currently provide
binaries for other host operating systems.
