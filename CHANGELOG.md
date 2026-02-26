# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Default to running all three providers (Claude, Codex, Gemini)
- `--claude` flag for single-provider mode (symmetry with `--codex` and `--gemini`)
- Parallel provider checks with per-provider progress spinners
- Table-formatted output with Provider, Limit, Remaining, Days, Minutes, Hours, and Spend columns
- `reset_hours` and `reset_days` derived fields in JSON output
- Dialog detection and auto-dismissal for trust, update, auth, and terms prompts
- `--approval-policy` flag (fail|accept) to control dialog handling
- `--cleanup` flag to kill stale tmux sessions
- JSON warnings as provider-keyed object (clean stdout in `--json` mode)
- `#![deny(warnings)]` for compile-time static analysis
- Library crate (`agentusage`) extracted for use as a dependency

### Changed
- Provider checks run in parallel instead of sequentially

### Fixed
- Auth dialog detection uses specific phrase matching to avoid false positives (e.g., "Authenticated as..." no longer triggers auth dialog)
- Gemini CLI prompt and dialog detection hardened for v0.28+
- Reset time parser supports compact formats without spaces (e.g., `Resets10pm(...)`)
- PTY `openpty` call passes explicit mut winsize pointer for correctness
- Codex update prompts now reliably dismiss via non-update options (Skip), avoiding automatic updates on behalf of users
