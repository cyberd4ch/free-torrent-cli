# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Test workflow (`.github/workflows/test.yml`) running the full suite on every push and pull request across Node 18/20/22
- Quick Start, GUI feature list, and Roadmap sections in the README

### Fixed
- `release.yaml` was sitting at the repository root, where GitHub Actions ignores it — moved to `.github/workflows/release.yml` so tag builds, Releases, and Pages deployment actually run
- Electron app no longer forces DevTools open in packaged (production) builds

## [2.0.0]

### Added
- Electron GUI with concurrent download dashboard, seeding state, and show-in-folder
- Full BitTorrent wire protocol, UDP + HTTP trackers, multi-file torrents, resume, and seeding
- From-scratch test suite (55 tests)
