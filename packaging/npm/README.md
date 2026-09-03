# remote-installer

Run the macOS `remote-installer` CLI without installing a Rust toolchain:

```bash
npx --yes @icodesign/remote-installer share /path/to/MyApp.ipa
# or a signed standalone Android APK
npx --yes @icodesign/remote-installer share /path/to/MyApp.apk
```

The package carries arm64 and x86_64 macOS binaries and selects the matching
one at startup. It supports signed iOS device builds and signed standalone
Android APKs; it is not a Simulator installer and does not currently provide
binaries for other host operating systems.
