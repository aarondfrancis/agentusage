# agentusage

Check [Claude Code](https://docs.anthropic.com/en/docs/claude-code), [Codex](https://openai.com/index/introducing-codex/), and [Gemini CLI](https://github.com/google-gemini/gemini-cli) usage limits from your terminal.

Launches each CLI tool in an isolated pseudo-terminal (`openpty`), runs its usage/status command, parses the TUI output, and reports usage percentages, reset times, and spend in a unified format.

**Important:** This tool works by driving your locally installed CLI tools. It does not call any provider APIs directly. This is intentional — calling protected usage/billing APIs from a non-first-party service risks getting your account banned. By using the official CLIs as the interface, agentusage stays within each provider's terms of service.

## Requirements

- `openpty` support (built into macOS/Linux)
- One or more AI coding CLI tools installed and authenticated:
  - `claude` (Claude Code)
  - `codex` (OpenAI Codex)
  - `gemini` (Gemini CLI)

Check your setup with:

```
agentusage --doctor
```

## Install

### From source

```
cargo install --path .
```

### From releases

Download a prebuilt binary from [Releases](https://github.com/aarondfrancis/agentusage/releases) for your platform:

- `agentusage-x86_64-apple-darwin` (Intel Mac)
- `agentusage-aarch64-apple-darwin` (Apple Silicon)
- `agentusage-x86_64-unknown-linux-gnu` (Linux x86)
- `agentusage-aarch64-unknown-linux-gnu` (Linux ARM)

## Usage

```
agentusage [OPTIONS]
```

By default, checks all installed providers in parallel and reports results:

```
$ agentusage

Claude Code Usage
────────────────────────────────────────────────────────
Current session:                   1% used · Resets 2pm (America/Chicago)
Current week (all models):         0% used · Resets Feb 20 at 9am (America/Chicago)
Extra usage:                      15% used · $77.33 / $500.00 spent · Resets Mar 1 (America/Chicago)

Codex Usage
────────────────────────────────────────────────────────
5h limit:                         97% left · resets 11:07
Weekly limit:                     71% left · resets 12:07 on 16 Feb

Gemini Usage
────────────────────────────────────────────────────────
gemini-2.5-pro:                   98% left · Resets in 2h 35m
gemini-2.5-flash:                 99% left · 6 reqs · Resets in 4h 49m
```

### Single provider

```
agentusage --claude
agentusage --codex
agentusage --gemini
```

### JSON output

```
agentusage --json
```

```json
{
  "success": true,
  "results": {
    "claude": {
      "Current session": {
        "percent_used": 1,
        "percent_remaining": 99,
        "reset_info": "Resets 2pm (America/Chicago)",
        "reset_minutes": 480
      }
    },
    "codex": {
      "5h limit": {
        "percent_used": 3,
        "percent_remaining": 97,
        "reset_info": "resets 11:07",
        "reset_minutes": 120
      }
    }
  }
}
```

When some providers fail but others succeed, warnings appear as a keyed object:

```json
{
  "success": true,
  "results": { "claude": { ... } },
  "warnings": {
    "codex": "codex CLI not found."
  }
}
```

### JSON fields

| Field | Type | Description |
|-------|------|-------------|
| `percent_used` | `u32` | Percentage of quota consumed (0-100) |
| `percent_remaining` | `u32` | Percentage of quota remaining (0-100) |
| `reset_info` | `string` | Raw reset text from the provider |
| `reset_minutes` | `i64?` | Minutes until reset (omitted if unparseable) |
| `spent` | `string?` | Spend info, e.g. `$77.33 / $500.00 spent` (Claude only) |
| `requests` | `string?` | Request count (Gemini only) |

## Options

| Flag | Description |
|------|-------------|
| `--claude` | Check only Claude Code |
| `--codex` | Check only Codex |
| `--gemini` | Check only Gemini CLI |
| `--json` | Output as machine-readable JSON |
| `--timeout <SECS>` | Max seconds to wait for data (default: 45) |
| `--verbose` | Print debug info (raw captured text, timing) |
| `--approval-policy <POLICY>` | Handle interactive dialogs: `fail` (default) or `accept` |
| `-C, --directory <DIR>` | Working directory for CLI sessions |
| `--cleanup` | Kill tracked agentusage PTY child sessions and exit |
| `--doctor` | Check provider CLIs |

## Dialog handling

CLI tools sometimes show interactive prompts (trust folder, update available, terms acceptance, authentication). By default, agentusage fails with an informative error when a dialog is detected.

Use `--approval-policy accept` to automatically dismiss dialogs that can be accepted with Enter (trust, update, terms, sandbox). Authentication and first-run dialogs always require manual resolution.

## Exit codes

| Code | Meaning |
|------|---------|
| 0 | Success |
| 1 | General error |
| 2 | Required tool not found (provider CLI) |
| 3 | Timeout waiting for provider output |
| 4 | Failed to parse provider output |

## How it works

1. Creates an isolated PTY session (`openpty`)
2. Launches the CLI tool and waits for its prompt
3. Sends the usage/status command (`/usage` for Claude, `/status` for Codex, `/stats session` for Gemini)
4. Polls PTY output until usage data appears
5. Parses percentages, reset times, and spend from the TUI output
6. Cleans up the process/session on exit (including Ctrl+C)

Each provider runs in its own PTY session. When checking all providers, they run in parallel.

## Library usage

agentusage can be used as a Rust dependency to programmatically check usage limits:

```rust
use agentusage::{run_claude, run_all, UsageConfig, ApprovalPolicy};

let config = UsageConfig {
    timeout: 45,
    verbose: false,
    approval_policy: ApprovalPolicy::Fail,
    directory: None,
};

// Single provider
let data = run_claude(&config)?;
for entry in &data.entries {
    println!("{}: {}% used", entry.label, entry.percent_used);
}

// All providers
let all = run_all(&config);
for data in &all.results {
    println!("{}: {} entries", data.provider, data.entries.len());
}
```

Add to your `Cargo.toml`:

```toml
[dependencies]
agentusage = { git = "https://github.com/aarondfrancis/agentusage" }
```

Key types re-exported at crate root: `UsageConfig`, `AllResults`, `UsageData`, `UsageEntry`, `ApprovalPolicy`, `PercentKind`.

## Development

```
cargo build
cargo test
```

190+ tests cover parsing, PTY session hardening, reset time computation, CLI flag validation, JSON serialization, and dialog detection.

## License

MIT
