---
layout: default
title: Terminal UI (TUI)
parent: Features
nav_order: 8
---

# Terminal UI (TUI)

Rift includes an interactive terminal user interface (`rift-tui`) for managing imposters and stubs without writing any code or API calls.

---

## Quick Start

```bash
# Build the TUI
cargo build --release --bin rift-tui

# Run (connects to localhost:2525 by default)
./target/release/rift-tui

# Connect to a different admin URL
./target/release/rift-tui --admin-url http://localhost:2525
```

---

## Features

- **Imposter Management** - View, create, edit, and delete imposters
- **Stub Editor** - JSON editor with syntax highlighting and validation
- **Search & Filter** - Find imposters and stubs quickly
- **Import/Export** - Load and save imposter configurations
- **Curl Generation** - Generate curl commands for testing stubs
- **Metrics Dashboard** - View request counts and statistics
- **Vim-style Navigation** - Navigate with j/k keys

---

## CLI Options

```bash
rift-tui [OPTIONS]

Options:
  -a, --admin-url <ADMIN_URL>    Admin API URL [env: RIFT_ADMIN_URL] [default: http://localhost:2525]
  -r, --refresh-ms <REFRESH_MS>  Refresh interval in milliseconds [default: 1000]
  -h, --help                     Print help
  -V, --version          Print version
```

---

## Keyboard Shortcuts

### Navigation

| Key | Action |
|:----|:-------|
| `j` / `↓` | Move down |
| `k` / `↑` | Move up |
| `Enter` | Select / Drill down |
| `Esc` | Go back / Close overlay |
| `Tab` | Switch focus between panes |
| `r` | Refresh data |
| `/` | Search / Filter |
| `T` | Cycle theme (Default / Dark / Light / Nord / Dracula) |
| `L` | Show recent errors and warnings |
| `?` | Toggle help |
| `q` | Quit (from main view); go back elsewhere |

### Imposter List

| Key | Action |
|:----|:-------|
| `n` | Create new imposter |
| `p` | Create proxy imposter |
| `d` | Delete selected imposter |
| `t` | Toggle enable/disable |
| `m` | View metrics dashboard |
| `C` | View server config (`GET /config`) |
| `i` | Import from file |
| `I` | Import from folder |
| `e` | Export all to file |
| `E` | Export to folder, one `<port>.json` per imposter (usable as a `--datadir`) |

### Imposter Detail

| Key | Action |
|:----|:-------|
| `a` | Add new stub |
| `e` | Edit selected stub |
| `d` | Delete selected stub |
| `D` | Duplicate selected stub |
| `[` / `]` | Move selected stub up / down |
| `y` | Copy stub as curl command |
| `t` | Toggle imposter enable/disable |
| `c` | Clear recorded requests |
| `C` | Clear proxy recordings |
| `x` | Export stubs only |
| `X` | Export full config |
| `A` | Apply recorded stubs |

### Stub Detail

| Key | Action |
|:----|:-------|
| `e` | Edit stub |
| `d` | Delete stub |
| `D` | Duplicate stub |
| `y` | Copy as curl command |

### JSON Editor

| Key | Action |
|:----|:-------|
| `Ctrl+S` | Save changes |
| `Ctrl+F` | Format JSON |
| `Ctrl+L` | Show full lint results |
| `Ctrl+A` | Select all |
| `Ctrl+C` | Copy selection |
| `Ctrl+X` | Cut selection |
| `Ctrl+V` | Paste |
| `Ctrl+K` | Delete line |
| `Ctrl+U` | Clear line before cursor |
| `Shift+Arrows` | Extend selection |
| `Ctrl+←/→` | Move by word |
| `Esc` | Cancel editing |

### Search Mode

| Key | Action |
|:----|:-------|
| `Enter` | Confirm search |
| `Esc` | Cancel search |
| `Ctrl+U` | Clear search query |
| `Ctrl+V` | Paste into search |

### Export Overlay

| Key | Action |
|:----|:-------|
| `s` | Save to file |
| `c` | Copy to clipboard |
| `j` / `k`, `↑` / `↓`, `PgUp` / `PgDn` | Scroll |
| `Esc` | Close |

---

## Creating Imposters

### Regular Imposter

Press `n` to create a new imposter:

1. **Port** (optional) - Leave empty for auto-assign
2. **Name** (optional) - Display name for the imposter
3. **Protocol** - http (default) or https

### Proxy Imposter

Press `p` to create a proxy imposter for recording:

1. **Target URL** (required) - Backend URL to proxy to
2. **Port** (optional) - Leave empty for auto-assign
3. **Name** (optional) - Display name
4. **Proxy Mode**:
   - `proxyOnce` - Record first response, replay subsequent
   - `proxyAlways` - Always forward, keep recording
   - `proxyTransparent` - Always forward, no recording

---

## Editing Stubs

The JSON editor provides:

- **Syntax highlighting** for JSON
- **Validation** - Errors shown before saving
- **Auto-format** - Press `Ctrl+F` to format JSON
- **Selection support** - Select with Shift+arrows, copy/paste

Example stub:

```json
{
  "predicates": [
    {
      "equals": {
        "method": "GET",
        "path": "/api/users"
      }
    }
  ],
  "responses": [
    {
      "is": {
        "statusCode": 200,
        "headers": {
          "Content-Type": "application/json"
        },
        "body": {
          "users": []
        }
      }
    }
  ]
}
```

---

## Search & Filter

Press `/` to activate search mode:

- Type to filter imposters/stubs
- Matching items stay highlighted
- Non-matching items are dimmed
- Press `Enter` to confirm and select first match
- Press `Esc` to clear search

Search matches against:
- Imposter port and name
- Stub predicates (path, method)
- Response content

---

## Curl Generation

Press `y` on any stub to generate a curl command:

```bash
curl -s \
  -X POST \
  -H 'Content-Type: application/json' \
  -d '{"userId": 123}' \
  'http://localhost:4545/api/users'
```

The command is copied to clipboard and can be pasted directly into a terminal.

The generator handles:
- Path extraction from predicates
- Method detection (GET, POST, etc.)
- Header extraction
- Body generation from JSONPath predicates
- Query parameter building

---

## Import/Export

### Import

| Action | Key | Description |
|:-------|:----|:------------|
| Import file | `i` | Load single imposter JSON |
| Import folder | `I` | Load all JSON files from folder |

### Export

| Action | Key | Description |
|:-------|:----|:------------|
| Export all | `e` | Save all imposters to single file |
| Export folder | `E` | Save each imposter to `<port>.json` in a folder |
| Export stubs | `x` | Export stubs without proxy responses |
| Export full | `X` | Export complete imposter config |

---

## Metrics Dashboard

Press `m` to view the metrics dashboard:

- Total request count across all imposters
- Per-imposter request counts
- Visual bar charts showing relative traffic
- Auto-refresh at the `--refresh-ms` interval (default every second)

---

## Tips

1. **Use search** (`/`) to quickly find imposters in large configurations
2. **Generate curls** (`y`) to test stubs from terminal
3. **Create proxy imposters** (`p`) to record real API traffic
4. **Apply recordings** (`A`) to convert proxy responses to static stubs
5. **Format JSON** (`Ctrl+F`) before saving to catch syntax errors

---

## See Also

- [rift-verify]({{ site.baseurl }}/features/stub-analysis/) - Automated stub testing
- [rift-lint]({{ site.baseurl }}/features/linting/) - Configuration validation
- [REST API]({{ site.baseurl }}/api/) - Programmatic imposter management
