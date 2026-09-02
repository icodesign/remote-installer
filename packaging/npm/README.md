# remote-installer

Run the macOS `remote-installer` CLI without installing a Rust toolchain:

```bash
npx --yes @icodesign/remote-installer share /path/to/MyApp.ipa
```

The package carries arm64 and x86_64 macOS binaries and selects the matching
one at startup. It is not a Simulator installer and does not support other
operating systems.
