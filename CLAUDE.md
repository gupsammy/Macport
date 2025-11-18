# macport

macOS menu bar app for monitoring and killing processes on development ports. Built with Rust and native system tray integration.

## Tech Stack

**Language:** Rust 2024 edition
**Package Manager:** Cargo
**Key Dependencies:** tray-icon, winit, nix, crossbeam-channel, anyhow

## Development Commands

```bash
# Run with debug logging
RUST_LOG=debug cargo run

# Build release binary
cargo build --release

# Code quality
cargo fmt              # Format code
cargo clippy           # Lint
cargo check            # Quick compile check

# Install git hooks (auto-format on commit)
./scripts/install-hooks.sh
```

## Helper Scripts

The `scripts/` directory contains utilities for development and testing:

- **install-hooks.sh**: Installs git pre-commit hook for automatic code formatting
- **start-test-ports.sh**: Spawns test processes on various ports for development testing
- **test-notifications.sh**: Tests macOS notification functionality

## Configuration

User config stored at `~/.macport.json` (auto-created on first run):

```json
{
  "port_ranges": [[3000, 3010], [5432, 5432], ...],
  "inactive_color": [255, 255, 255],    // Menu bar icon when idle
  "active_color": [255, 69, 58],        // Icon when ports active
  "notifications_enabled": true
}
```

Edit via menu: **Edit Configuration** opens in default text editor.

## Architecture

### Module Organization
Domain-driven modular architecture with clear separation of concerns:

```
src/
├── main.rs              # Entry point
├── lib.rs               # Module exports
├── app.rs               # Application orchestration, event loop
├── config.rs            # Configuration management (~/.macport.json)
├── model.rs             # Core data structures (AppState, ProcessInfo)
├── notify.rs            # macOS notification integration
├── process/
│   ├── ports.rs         # Port scanning via lsof
│   └── kill.rs          # Process termination logic
├── ui/
│   ├── icon.rs          # Tray icon rendering
│   └── menu.rs          # Menu construction and updates
└── integrations/
    ├── docker.rs        # Docker container detection
    └── brew.rs          # Homebrew service detection
```

### Threading Model
Four concurrent threads communicate via channels and event loop proxy:

1. **Main Loop** (winit): UI events, tray updates, state orchestration
2. **Monitor Thread**: Polls ports every 2s via `process/ports.rs`, detects Docker/Brew services
3. **Menu Listener**: Converts menu clicks to actions
4. **Kill Worker**: Executes termination via `process/kill.rs`

### Core Responsibilities

**app.rs**: Event loop orchestration, thread management, state coordination
**config.rs**: Load/save user preferences, default port ranges
**model.rs**: `AppState` (processes, Docker/Brew mappings, project cache, snooze state)
**process/ports.rs**: Parse `lsof` output, map ports to PIDs
**process/kill.rs**: Graceful shutdown (SIGTERM → 2s → SIGKILL → 1s)
**ui/menu.rs**: Dynamic menu with process/Docker/Brew items
**ui/icon.rs**: Template-based icon with configurable colors
**integrations/docker.rs**: Map container names to exposed ports
**integrations/brew.rs**: Detect and verify Homebrew services on ports

### Menu Actions
- **Kill [process]**: Terminate specific PID
- **Kill all**: Terminate all monitored processes
- **Stop [docker container]**: `docker stop <container>`
- **Stop [brew service]**: `brew services stop <service>`
- **Snooze 30m**: Pause monitoring temporarily
- **Edit Configuration**: Open `~/.macport.json`
- **Quit**: Exit app

## Default Port Ranges

```rust
(3000, 3010)   // Node.js, React, Next.js, Vite
(3306, 3306)   // MySQL
(4000, 4010)   // Alternative Node servers
(5001, 5010)   // Flask, dev servers (5000 excluded - macOS AirPlay)
(5173, 5173)   // Vite
(5432, 5432)   // PostgreSQL
(6379, 6379)   // Redis
(8000, 8100)   // Django, Python
(8080, 8090)   // Tomcat, alt HTTP
(9000, 9010)   // Dev tools
(27017, 27017) // MongoDB
```

Note: Port 5000 excluded to avoid conflicts with macOS AirPlay Receiver.

## Key Constants

```rust
POLL_INTERVAL = 2s           // Monitor frequency
SIGTERM_GRACE = 2s           // Before SIGKILL
SIGKILL_GRACE = 1s           // Final grace period
POLL_STEP = 200ms            // Process check granularity
MAX_TOOLTIP_ENTRIES = 5      // Max displayed in tooltip
```

## Common Patterns

### Adding a monitored port range
Edit `~/.macport.json` via menu or directly. Changes require restart. Default ranges defined in `config.rs`.

### Extending integrations
Add new service detection to `src/integrations/`. Follow pattern: detection function, port mapping, menu integration.

### Debugging
- **Port detection**: `RUST_LOG=debug cargo run` shows `lsof` parsing
- **Docker/Brew**: Debug logs show container/service discovery
- **Process termination**: Logs show SIGTERM/SIGKILL sequence

## Development Notes

- Pre-commit hook auto-formats code (install via `./scripts/install-hooks.sh`)
- Menu IDs use prefixes: `process_`, `docker_stop_`, `brew_stop_` for action routing
- All external identifiers sanitized to prevent injection attacks
- Icon updates on state changes (not timer-based) for efficiency
- Project cache in `model.rs` prevents repeated git lookups
- Brew service detection verifies actual port binding (not just service status)

## Coding Principles

- **Module boundaries**: Keep domain logic isolated (process/, ui/, integrations/)
- **Error handling**: Use `anyhow::Result` for operations that can fail
- **Logging**: Use `log::debug!` for diagnostics, `log::error!` for failures
- **Thread safety**: Communicate via channels, minimize shared state
- **Platform commands**: Sanitize all inputs to `lsof`, `docker`, `brew`, `osascript`
