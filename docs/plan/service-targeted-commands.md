# Service-targeted lifecycle commands Design Document

## Background & Goals

### Problem
`up` only accepts the whole configuration, background processes cannot be restarted through the CLI, and `logs` cannot use Docker Compose's `-f` spelling because that short flag is allocated to the global configuration-file option.

### Success Criteria
- `up [SERVICE...]` starts the requested services and their transitive dependencies in dependency order; unrelated services do not start.
- `restart [SERVICE...]` restarts every named background-session service, including an exited recorded service, or every recorded session service when omitted.
- `logs -f [SERVICE...]` follows only the requested services; `logs [SERVICE...]` shows their history.
- Unknown service names fail before spawning, signaling, or reading logs.

## High-Level Design

`Config` will supply a service-selection helper that validates names and closes the selection over `depends_on`. `up` receives this set and limits each dependency wave to selected names; the existing readiness and failure propagation paths remain unchanged.

`restart` operates only on the stored background session. For each selected live PID it signals the process group, waits for exit under the configured timeout, then relaunches the matching configured service into its existing log file and persists the replacement PID. An exited recorded service skips stopping and is relaunched.

The global config option becomes long-only `--file`, releasing `-f` for `logs --follow`. `--follow` remains the descriptive long spelling.

## Implementation Plan

### Stage 1: Selected-service closure
- **Files modified**: `src/config.rs`, `src/main.rs`
- **Specific logic**: Add a validated, transitive `depends_on` selection helper. Filter foreground and detached startup waves with its result. Pass positional service names from `UpArgs`.
- **Validation**: Unit-test selection of a dependent, missing name rejection, and an empty selection; integration-test an `up -d` target excludes unrelated services.

### Stage 2: Background restart
- **Files modified**: `src/main.rs`, `src/session.rs` if a focused helper reduces duplication
- **Specific logic**: Add `restart [SERVICE...]`, validate selected session services, stop selected running process groups with existing timeout behavior, relaunch from loaded config into existing logs, then persist each successful replacement PID. On launch failure, report the stopped service rather than silently treating it as running.
- **Validation**: Integration-test selected restart changes only the selected PID and preserves other live services; test unknown targets fail without disturbing other services.

### Stage 3: Compose-style log follow
- **Files modified**: `src/main.rs`
- **Specific logic**: Reserve `-f` for `LogsArgs::follow`; retain `--file` for explicit configuration selection.
- **Validation**: Integration-test `logs -f SERVICE` receives subsequent output for that service only.

### Stage 4: Documentation and regression
- **Files modified**: `README.md`, `docs/IMPLEMENTATION_PLAN.md`, `docs/plan/cue.md`, `tests/background.rs`
- **Specific logic**: Document service selection, dependency inclusion, restart scope, and the `--file` long-option cutover.
- **Validation**: Run focused integration tests, `cargo test`, and `cargo clippy -- -D warnings`.

## Testing Strategy
- Selected `up`: direct target, transitive dependency, unrelated exclusion, unknown name rejection.
- Restart: selected and all-service scopes, graceful stop and fresh PID, configuration/session mismatch failures.
- Logs: history filtering and short `-f` follow filtering.
- Regression: existing background lifecycle, dependency orchestration, and CLI options.

## Risks & Mitigation
- Restart needs config data while `down` intentionally does not. Keep that distinction: `restart` loads and validates configuration because it must relaunch.
- Killing a process before replacement spawn risks a stopped service on launch failure. Surface an explicit error and persist only the successfully spawned PIDs; callers can retry once the configuration is fixed.
- `-f` is a breaking reassignment from global config path to log following. Keep `--file` fully supported and document the cutover.

## Rollback Plan
Remove the command and selection wiring; existing whole-session lifecycle behavior remains intact.
