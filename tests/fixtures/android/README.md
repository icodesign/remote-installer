# Android fixture

`signed-fixture.apk` is a minimal standalone APK built from
`AndroidManifest.xml` with Android Build Tools 36.0.0. It has no executable
code and exists only for package inspection and HTTP download tests.

It is signed with an ephemeral test-only RSA key. The expected signer
certificate SHA-256 digest is:

```text
95f3fc3ee59a9d33792c2fb0b8bebd63836b312e30f03d8db5855bd98731a5b7
```

The private key is deliberately not kept in this repository.
