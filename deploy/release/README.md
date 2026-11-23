# Host releases

`package-host` extracts the executable from the Linux amd64 deployment image, checks its source-revision label, and packages the bytes into a Cargo Binstall archive. Use the image ID reported by the running deployment. The binary, release manifest, public keys and build record are covered by the signed checksums (the binary via its archive). Cargo Binstall verifies the archive's Minisign signature with the public key in the package manifest.

From the release checkout, with Python 3.11+, Docker, Minisign and GnuPG:

```sh
deploy/release/package-host "$IMAGE_ID" "$SOURCE_COMMIT" "$OUTPUT_DIR" --minisign-key "$MINISIGN_KEY"
```

The output directory must be new. The OpenPGP secret key matching `plexus-agents-release.asc` must be available to GnuPG. For noninteractive signing, `--gpg-passphrase-fd N` reads from an inherited file descriptor. Stage the files on a draft GitHub Release for `v<package version>`, then publish it. Verify a fresh signed installation using the command in the repository README.

To verify the OpenPGP checksums independently, download the release files, then run:

```sh
gpg --dearmor --output release.gpg plexus-agents-release.asc
gpgv --keyring ./release.gpg SHA256SUMS.asc SHA256SUMS
sha256sum --check SHA256SUMS
```

OpenPGP fingerprint: `3ED6 899A 3267 FA41 5F4C DA99 9493 52B5 30DD E55A`.
