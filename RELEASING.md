# Releasing

The version lives in exactly one place: `[workspace.package] version` in
`Cargo.toml`. Both crates inherit it, and the binary, `ftc --version` and the
GUI's `/api/version` all read it at compile time through `CARGO_PKG_VERSION`,
so nothing else in the code carries a number to bump.

A tag is the only thing that publishes. `release.yml` fires on `v*`, builds the
four platform archives plus the macOS app bundle, writes `SHA256SUMS`, and
attaches everything to a GitHub Release. `static.yml` deploys `docs/` on every
push to `main`.

## Before the tag

1. Bump `[workspace.package] version` in `Cargo.toml` and let cargo refresh
   `Cargo.lock` (`cargo metadata --format-version 1 >/dev/null`).
2. Close the changelog section: change `## [Unreleased]` to
   `## [X.Y.Z] - YYYY-MM-DD`, and leave a fresh empty `## [Unreleased]` above
   it for the next cycle.
3. Date the post: in `docs/release-X.Y.Z.html`, set the kicker to
   `Release X.Y.Z` and the meta line to `YYYY-MM-DD · dach-a · ...`, and delete
   any "not tagged yet" callout left from the draft.
4. Move the site onto the new version: the hero badge and `platform-version`
   lines in `docs/index.html`, plus the nav and footer links (keep the previous
   post linked — old notes are not deleted). Delete the tag-pending sentence
   under the download buttons: once the tag exists, that caveat is false.
5. Point readers at the newest notes: `README.md`'s site line, and the footer
   entry on `docs/index.html`.
6. Green gate, same as CI: `cargo test --all`,
   `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`,
   `shellcheck install.sh`.

## Tag

```bash
git tag -a vX.Y.Z -m "Footorrent X.Y.Z"
git push origin vX.Y.Z
```

## After the tag

- Watch the `release` workflow. It must produce `ftc-linux-x86_64.tar.gz`,
  `ftc-linux-arm64.tar.gz`, `ftc-macos-arm64.tar.gz`, `ftc-macos-x86_64.tar.gz`,
  `ftc-windows-x86_64.zip`, `ftc-macos-app.zip` and `SHA256SUMS`.
- Confirm the macOS bundle is universal: `lipo -archs
  Footorrent.app/Contents/MacOS/ftc-gui` should list `x86_64 arm64`.
- Install once from the published artifacts, not from a local build:
  `curl -fsSL https://raw.githubusercontent.com/cyberd4ch/footorrent/main/install.sh | sh`.
  The digest line must read `-> sha256 ok`. Any `-> sha256 SKIPPED — …` line
  (printed on stderr) means the release is missing `SHA256SUMS`, the host has
  no `shasum`/`sha256sum`, or the check was opted out of — read the reason.
- On macOS, confirm `Footorrent.app` opens the window and shows the new dock
  icon (`assets/brand/AppIcon.icns` is packed into the bundle by `release.yml`;
  `shasum -a 256 Contents/Resources/AppIcon.icns` should match the repo copy).
  The bundle is Apple silicon only — `install.sh` writes a browser launcher on
  Intel, so an Intel Mac is not a failing check.
- Confirm the Pages run that follows the changelog/site commit deployed, and
  that the release post resolves at `docs/release-X.Y.Z.html`.
- Only if the workflow failed before publishing anything: delete the tag
  locally and on the remote, fix, and re-tag. Never move a tag that already has
  a Release attached — download URLs are cached by CDNs and by installers.
