# Cue Windows Support Design Document

## Background & Goals

### Problem
Cue's Unix process-group lifecycle works end to end, but the existing non-Unix fallback treats every PID as exited and invokes `taskkill` for every stop. As a result, Windows cannot correctly report, restart, or stop background services.

### Success Criteria
- Windows supports `up`, `up -d`, `ps`, `logs -f`, targeted `restart`, and `down` with the same observable contract as macOS/Linux.
- A Windows background session records each process's creation time and refuses to treat a reused PID as the managed service.
- Graceful shutdown first sends `CTRL_BREAK_EVENT` to the service's own process group; timeout and `--stop-timeout 0` force-terminate the complete process tree with `taskkill /F /T`.
- Windows integration tests execute in CI; Unix integration tests remain unchanged.

## High-Level Design

Cue keeps the shared orchestration, configuration, logging, and session code. A small platform lifecycle layer supplies process-group creation and two stop modes:

- Unix creates a new process group and sends `SIGTERM` then `SIGKILL` to the negative group PID.
- Windows creates a new process group. Graceful control tries `GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid)` in the current console and, when the caller is in another console, temporarily attaches to the service console before retrying. Forced control uses the native `taskkill /F /T /PID` tree termination path.

A Windows Job Object is deliberately not used for detached services. A Job Object survives only while a handle is open; `up -d` exits and cannot retain that handle without introducing a persistent supervisor process. Windows' standard tree termination plus identity validation preserves the current single-process CLI architecture.

`SessionService` adds an optional process creation timestamp. It is populated only on Windows using `OpenProcess` and `GetProcessTimes`. Background lifecycle operations use the full service record, not just the PID, so an absent or mismatched timestamp is treated as exited rather than risking a PID-reuse kill.

## Implementation Plan

### Stage 1: Platform lifecycle abstraction
- **Files modified**: `Cargo.toml`, `src/runner.rs`, `src/main.rs`
- **Specific logic**: Create a Windows process group at spawn; replace signal-number plumbing with platform-neutral graceful and force stop functions. Preserve Unix signals and use Windows Ctrl+Break then `taskkill /F /T`.
- **Validation**: Unix regression suite passes; Windows CI compiles the Windows implementation and executes its lifecycle test.

### Stage 2: Reliable Windows session state
- **Files modified**: `src/session.rs`, `src/main.rs`
- **Specific logic**: Bump the state schema, persist Windows creation timestamps, validate service identity before `ps`, `down`, and `restart`, and store session state under `LOCALAPPDATA\\cue` on Windows.
- **Validation**: Windows background test observes a running service, replaces its PID through restart, and removes state through down.

### Stage 3: Windows behavior coverage and CI
- **Files modified**: `tests/windows_background.rs`, `.github/workflows/ci.yml`
- **Specific logic**: Add a Windows-only `cmd.exe` fixture created at runtime. Cover detached startup, status, filtered historical/followed logs, targeted restart, and forced tree shutdown. Run `cargo test` and `cargo clippy -- -D warnings` on Ubuntu and Windows.
- **Validation**: GitHub Actions runs both operating systems for pushes and pull requests.

### Stage 4: Documentation
- **Files modified**: `README.md`, `docs/IMPLEMENTATION_PLAN.md`, `docs/plan/cue.md`
- **Specific logic**: State full Windows lifecycle support and explain the platform-specific shell and graceful-stop behavior.
- **Validation**: Documentation contains no obsolete Windows compile-only claim.

## Testing Strategy
- Windows: `up -d → ps → logs → logs -f → restart → down`, plus process-identity detection.
- Unix: existing foreground signal, dependency, interpolation, detached lifecycle, and selected-service tests.
- CI: execute both suites and clippy on `ubuntu-latest` and `windows-latest`.

## Risks & Mitigation
- Console control events only reach a process group associated with a console. Windows retry logic attaches to the target's console; services that ignore Ctrl+Break receive bounded waiting followed by forced tree termination.
- Windows `taskkill` follows the parent-child process tree. A child that deliberately detaches is outside the tree, equivalent to a Unix child creating a separate session; document this boundary.
- PID reuse is unavoidable with PID-only state. Creation-time validation fails safely rather than targeting an unrelated process.

## Rollback Plan
Remove the Windows lifecycle module paths and CI job, retaining the existing Unix implementation. State version changes naturally invalidate old sessions.
