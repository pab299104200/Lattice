# Lattice Claude Code Integration

This package installs Claude Code hooks that deliver Lattice context without relying on the model to choose an MCP tool.

Install from the repository root:

```bash
integrations/claude-code/install.sh
```

Implementation choice: Lattice uses this `install.sh` package instead of adding `lattice install claude-code`. The installer only merges hook entries into the project `.claude/settings.json`; it does not edit global Claude configuration.

## Hooks

- `SessionStart`: calls `lattice recall "session start" --mode task --json`, adds current task memory and repo rules as `additionalContext`, and clips output to about 1500 tokens.
- `UserPromptSubmit`: calls `lattice context "<prompt>" --mode auto --min-relevance 0.25`, clips output to about 1200 tokens, and emits nothing when the result is too small or not relevant.
- `PostToolUse`: for `Edit|Write`, calls `lattice impact <edited-file> --no-tests`, emits at most 10 lines, and skips leaf edits by default unless at least three impact/dependent lines are present.
- `Stop`: extracts edited files from the hook payload and calls `lattice remember --kind outcome` so the next session can recall the work.

Every script first checks whether the Lattice binary exists and whether the daemon answers `lattice status` within `LATTICE_HOOK_PROBE_TIMEOUT` seconds. If either check fails, the script exits `0` without output so hooks never break a Claude session.

## Controls

- Set `LATTICE_BIN=/absolute/path/to/lattice` to force a binary path.
- Set `LATTICE_HOOK_PROBE_TIMEOUT` to tune the readiness probe timeout; the default is `0.2` seconds.
- Set `LATTICE_HOOK_MIN_RELEVANCE` to tune prompt context filtering.
- Set `LATTICE_HOOK_MIN_DEPENDENTS` to tune PostToolUse noise filtering.
- Disable a hook by removing its entry from `.claude/settings.json`.
