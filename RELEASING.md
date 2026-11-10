# Releasing

The version lives in exactly one place: `[workspace.package] version` in
`Cargo.toml`. Both crates inherit it, and the binary, `ftc --version` and the
GUI's `/api/version` all read it at compile time through `CARGO_PKG_VERSION`,
so nothing else in the code carries a number to bump.

A tag is the only thing that publishes. `release.yml` fires on `v*`, builds the
five platform archives plus the macOS app bundle, writes `SHA256SUMS`, and
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

## Signing and notarizing

The `app-macos` job signs and notarizes when these repository secrets exist, and
falls back to the ad-hoc signature (`codesign --sign -`) when they do not — with
a `::notice` in the log, so an unsigned release is visible rather than silent.
A fork needs none of this; a release that should open without a Gatekeeper
warning needs all of it.

| Secret | What it is |
| --- | --- |
| `MACOS_CERT_P12` | `base64 -i developerID.p12` — a Developer ID Application certificate plus its private key, exported from Keychain Access |
| `MACOS_CERT_PASSWORD` | the `.p12` export password |
| `MACOS_KEYCHAIN_PASSWORD` | any throwaway password for the temporary keychain the job creates |
| `MACOS_SIGN_IDENTITY` | e.g. `Developer ID Application: Your Name (TEAMID)` |
| `MACOS_NOTARY_APPLE_ID` | the Apple ID that owns the team |
| `MACOS_NOTARY_TEAM_ID` | the 10-character team identifier |
| `MACOS_NOTARY_PASSWORD` | an app-specific password from appleid.apple.com, **not** the account password |

What the job does with them: unlock a temporary keychain, sign the nested
`Contents/MacOS/ftc` first (a `--deep` signature would leave it ad-hoc and the
notary rejects the bundle), sign the app with the hardened runtime, a timestamp
and `gui/macos/Footorrent.entitlements`, submit a zip with
`notarytool submit --wait`, staple the bundle, then build the shipped zip from
the **stapled** bundle and re-verify what that zip contains. It also deletes the
keychain on exit, including on failure.

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
- Check the version the artifacts report, not just the tag: from the
  downloaded `ftc-macos-app.zip`, `Contents/MacOS/ftc --version` must print the
  tag's number. 3.6.0 shipped from a `v3.7.0` tag once, because the workspace
  version was never bumped — `Cargo.toml` is the only place it lives.
- On macOS, confirm `Footorrent.app` opens the window and shows the new dock
  icon (`assets/brand/AppIcon.icns` is packed into the bundle by `release.yml`;
  `shasum -a 256 Contents/Resources/AppIcon.icns` should match the repo copy).
  Clicking through `ftc gui` in the bundle is the same page the CLI serves.
- On Windows, the Start Menu entry must open an Edge/Chrome app window — no
  address bar, no Back button — and closing it must stop the server. A console
  window appearing behind it means the shortcut launched a console that was
  shared with something else; check `GetConsoleProcessList` handling in
  `crates/cli/src/window.rs`.
- If signing is configured, `spctl --assess --type execute -vv` on the extracted
  app must say `accepted` and `source=Notarized Developer ID`, and
  `xcrun stapler validate` must pass *after* unzipping the shipped archive.
- Confirm the Pages run that follows the changelog/site commit deployed, and
  that the release post resolves at `docs/release-X.Y.Z.html`.
- Only if the workflow failed before publishing anything: delete the tag
  locally and on the remote, fix, and re-tag. Never move a tag that already has
  a Release attached — download URLs are cached by CDNs and by installers.
