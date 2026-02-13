# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Default to running all three providers (Claude, Codex, Gemini) sequentially
- `--claude` flag for single-provider mode (symmetry with `--codex` and `--gemini`)
- Dialog detection and auto-dismissal for trust, update, auth, and terms prompts
- `--approval-policy` flag (fail|accept) to control dialog handling
- `--cleanup` flag to kill stale tmux sessions
- JSON warnings as provider-keyed object (clean stdout in `--json` mode)
- `#![deny(warnings)]` for compile-time static analysis
- 96 unit tests
