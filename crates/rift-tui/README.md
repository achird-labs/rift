# rift-tui

Interactive terminal UI for [Rift](https://github.com/achird-labs/rift), driven entirely through
the admin API.

## Features

- **Imposter Management** - View, create, edit, and delete imposters
- **Stub Editor** - JSON editor with syntax highlighting and `rift-lint` validation
- **Search & Filter** - Find imposters and stubs quickly
- **Import/Export** - Load and save imposter configurations
- **Curl Generation** - Generate curl commands for testing stubs
- **Metrics Dashboard** - View request counts and statistics
- **Recordings** - Clear recorded requests and proxy recordings, apply recorded stubs
- **Vim-style Navigation** - Navigate with j/k keys

## Installation

`rift-tui` ships in every release archive and the Homebrew formula alongside `rift`. It is not
published to crates.io — to build it yourself:

```bash
git clone https://github.com/achird-labs/rift.git
cd rift
cargo build --release --bin rift-tui
```

The binary lands at `target/release/rift-tui`.

## Usage

```bash
# Connect to default admin URL (http://localhost:2525)
rift-tui

# Connect to a different admin URL (or set RIFT_ADMIN_URL)
rift-tui --admin-url http://rift.internal:2525

# Poll less often (default 1000 ms)
rift-tui --refresh-ms 5000
```

## Keyboard Shortcuts

### Navigation

| Key | Action |
|:----|:-------|
| `j` / `↓` | Move down |
| `k` / `↑` | Move up |
| `Enter` | Select / Drill down |
| `Esc` | Go back / Close |
| `Tab` | Switch panes |
| `r` | Refresh |
| `/` | Search |
| `T` | Cycle theme |
| `L` | Recent errors and warnings |
| `?` | Help |
| `q` | Quit |

### Imposter List

| Key | Action |
|:----|:-------|
| `n` | New imposter |
| `p` | New proxy imposter |
| `d` | Delete imposter |
| `t` | Toggle enable/disable |
| `m` | Metrics dashboard |
| `C` | Server config (`GET /config`) |
| `i` / `I` | Import file / folder |
| `e` / `E` | Export file / folder |

### Stub Management

| Key | Action |
|:----|:-------|
| `a` | Add stub |
| `e` | Edit stub |
| `d` | Delete stub |
| `D` | Duplicate stub |
| `[` / `]` | Move stub up / down |
| `y` | Copy as curl |
| `c` / `C` | Clear recorded requests / proxy recordings |
| `x` / `X` | Export stubs only / full config |
| `A` | Apply recorded stubs |

### Editor

| Key | Action |
|:----|:-------|
| `Ctrl+S` | Save |
| `Ctrl+F` | Format JSON |
| `Ctrl+A` | Select all |
| `Ctrl+C/X/V` | Copy/Cut/Paste |
| `Esc` | Cancel |

## Documentation

The complete keybinding list, including the editor and search modes, is in
[Terminal UI](https://achird-labs.github.io/rift/features/tui/).

## License

Apache-2.0 - see [LICENSE](../../LICENSE) for details.
