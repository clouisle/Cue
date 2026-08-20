# Cue Tagged Release Automation Design Document

## Background & Goals

### Problem to solve

Cue currently validates source on Ubuntu and Windows but publishes no ready-to-run binaries. Consumers must install Rust and build from source.

### Success criteria

- Pushing any Git tag triggers the release workflow; branch and pull-request updates never trigger it.
- The workflow builds `cue` from the locked dependency set for Linux x86_64, macOS Apple Silicon, and Windows x86_64.
- Each platform artifact is packaged with an unambiguous target-specific name and attached to the GitHub Release for the pushed tag.
- The release also contains a SHA-256 checksum manifest.

## High-Level Design

`.github/workflows/ci.yml` validates only branch pushes and pull requests; `.github/workflows/release.yml` is the sole tag-triggered workflow and uses `push.tags: ['*']`.

A three-entry build matrix installs the stable Rust toolchain with the corresponding compilation target, runs `cargo build --locked --release --target`, and packages just the resulting `cue` executable. Unix artifacts use `tar.gz`; Windows uses `zip`. Each archive is uploaded as a workflow artifact.

A dependent Ubuntu release job downloads all archives, writes `SHA256SUMS`, and invokes `softprops/action-gh-release` with `GITHUB_TOKEN` permission to create or update the release matching `github.ref_name`.

## Implementation Plan

### Stage 1: Define release architecture

- **Files modified**: `.github/workflows/ci.yml`, `docs/IMPLEMENTATION_PLAN.md`, `docs/plan/cue-tagged-releases.md`
- **Specific logic**: Record platform targets, event boundaries, artifact formats, and release permissions; restrict validation CI to branch pushes so tags execute only the release workflow.
- **Validation**: Confirm CI remains responsible for branch/PR validation and tags match only the release workflow.

### Stage 2: Build and package platform artifacts

- **Files modified**: `.github/workflows/release.yml`
- **Specific logic**: Add a three-platform build matrix, locked release builds, platform-native archive commands, and artifact upload.
- **Validation**: Parse the workflow and verify every matrix target, executable path, archive extension, and upload path agree.

### Stage 3: Publish releases and document use

- **Files modified**: `.github/workflows/release.yml`, `README.md`
- **Specific logic**: Add a release job with minimum `contents: write` permission, artifact download, checksums, generated release notes, and direct-release installation guidance.
- **Validation**: Confirm the workflow references only the tag name and contains no branch or pull-request trigger.

## Testing Strategy

- Static workflow validation: parse the YAML and, when available, run `actionlint`.
- Build contract: `cargo build --locked --release` locally validates the same locked release-build command used by all matrix entries.
- Release smoke test: pushing a disposable tag should produce four archives plus `SHA256SUMS` on the matching GitHub Release; delete the disposable tag and release afterward.
- Regression scope: existing `.github/workflows/ci.yml` continues validating pushes and pull requests independently.

## Risks & Mitigation

- **Runner architecture drift**: matrix targets identify the artifact architecture explicitly; `macos-13` and `macos-14` keep Intel and Apple Silicon builds separate.
- **Dependency drift**: every build uses `--locked`.
- **Corrupt or misidentified downloads**: release asset names include their target triple and `SHA256SUMS` permits verification.
- **Duplicate tag builds**: GitHub Release creation/update is keyed to the tag, so rerunning the workflow updates the same release rather than creating an ambiguous release.
- **Rollback plan**: disable or remove `release.yml`; no source or runtime behavior changes.
