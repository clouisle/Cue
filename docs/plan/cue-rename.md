# Cue Rename Design Document

## Background & Goals

### Problem
The tool still exposes its former identity in its package name, executable, configuration filename, session cache, fixtures, tests, and user documentation. The requested command is `cue up`, communicating a ready/start cue.

### Success Criteria
- Cargo builds an executable named `cue`; its usage and help identify it as Cue.
- Cue discovers only `.cue.json`; all project fixtures and documentation use that filename.
- Background session state lives below the Cue cache root, so old tool sessions cannot be mistaken for Cue sessions.
- No active source, test, fixture, README, or implementation-plan reference exposes the former project or configuration name.

## High-Level Design

This is a clean public-surface cutover. The Cargo package becomes `cue`, which produces the `cue` binary and changes Cargo's integration-test executable environment variable to `CARGO_BIN_EXE_cue`. The configuration discovery constant selects `.cue.json`; fixture files are renamed to match. The cache root becomes `cue`, isolating Cue's background state from previous tool sessions.

No compatibility aliases are retained: supporting a former command or configuration filename would leave a second public convention and does not meet the requested rename. Existing users migrate their configuration to `.cue.json` and invoke `cue`.

## Implementation Plan

### Stage 1: Runtime identity
- **Files modified**: `Cargo.toml`, `src/main.rs`, `src/config.rs`, `src/session.rs`, `Cargo.lock`
- **Specific logic**: Rename package/binary and CLI metadata; select `.cue.json` for discovery; change cache namespace to `cue`.
- **Validation**: Build `cue`, run `cue --help`, and assert missing-configuration errors name `.cue.json`.

### Stage 2: Test and fixture migration
- **Files modified**: `tests/*.rs`, `testdata/**/.cue.json`, test-only temporary paths
- **Specific logic**: Rename every fixture configuration to `.cue.json`, switch `CARGO_BIN_EXE_*`, and eliminate stale temporary-path identifiers.
- **Validation**: Run all unit and integration tests against the renamed executable and fixtures.

### Stage 3: Documentation cutover
- **Files modified**: `README.md`, `docs/IMPLEMENTATION_PLAN.md`, `docs/plan/*.md`
- **Specific logic**: Rename project/config/cache/command references and move the primary design document to its Cue identity.
- **Validation**: Search tracked source, tests, fixtures, and documentation for former public names.

## Testing Strategy
- CLI smoke: `cargo run -- --help` exposes `cue`; `cargo run -- up` in an empty directory reports `.cue.json`.
- Discovery: existing fixture directories continue to be found after `.cue.json` migration.
- Regression: `cargo test` and `cargo clippy -- -D warnings` pass.

## Risks & Mitigation
- The configuration filename and cache root are breaking changes. They are intentional clean-cutover scope; documentation gives the explicit migration.
- Cargo's test executable variable follows the package name. Update test harnesses atomically with the package rename.

## Rollback Plan
Restore the former package/config/cache strings and fixture filenames; no data migration is performed.
