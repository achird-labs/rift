---
layout: default
title: Node.js Integration
parent: Getting Started
nav_order: 4
permalink: /getting-started/nodejs/
---

# Node.js Integration

Rift provides official Node.js bindings via the `@rift-vs/rift` npm package. This package is a drop-in replacement for Mountebank's Node.js API, making migration seamless.

> **The Node.js SDK lives in its own repository.** It is developed and published from
> [`achird-labs/rift-node`](https://github.com/achird-labs/rift-node), which owns the npm
> release. This engine repository no longer contains or publishes it — file SDK issues there, and
> engine issues here.

---

## Installation

```bash
npm install @rift-vs/rift
```

There is **no `postinstall` download**. Nothing is fetched at `npm install` time — the engine
binary is resolved lazily, the first time it's actually needed (`rift.spawn()` or the
Mountebank-compat `create()`), in this order:

1. An explicit override — `binaryPath` passed to `spawn()`, or the `RIFT_BINARY_PATH` env var.
2. The system `PATH` — the first of `rift-http-proxy` / `rift` / `mb` found, but only if it
   `--version`-probes as Rift. A Mountebank `mb` shadowing the name on `PATH` is skipped rather
   than run in Rift's place.
3. A previously-downloaded copy in the local version cache.
4. A checksummed download from GitHub Releases (or `RIFT_DOWNLOAD_URL`/`RIFT_MIRROR_URL`), cached
   for next time.

If `RIFT_OFFLINE` or `RIFT_SKIP_BINARY_DOWNLOAD` is set, step 4 never runs — resolution throws with
manual-install instructions instead of touching the network. Run `npx rift-fetch` ahead of time
(e.g. in CI, or to prep an air-gapped install) to warm the cache so first use doesn't pay the
download cost.

The in-process **embedded** transport (`rift.embedded()`) resolves a separate artifact — the
`librift_ffi` cdylib — via the companion `@rift-vs/rift-embedded` package, following the same
override → cache → download order (see below).

### Supported Platforms

| Platform | Architecture | libc |
|:---------|:-------------|:-----|
| macOS    | x64, arm64   | -    |
| Linux    | x64, arm64   | glibc and musl (musl auto-selected on Alpine) |
| Windows  | x64          | -    |

---

## Basic Usage

The modern surface is a typed, fluent DSL over three transports that all hand back the same
engine client — pick whichever fits how the binary should run:

- `rift.embedded()` — in-process, no child process, needs `@rift-vs/rift-embedded` installed.
- `rift.spawn()` — manages the `rift` engine binary as a child process for you.
- `rift.connect(url)` — attaches to an engine already running elsewhere.

```javascript
import { rift, imposter, onGet, okJson } from '@rift-vs/rift';

await using engine = await rift.spawn(); // or rift.embedded() / rift.connect(url)

const users = await engine.create(
  imposter('users').stub(onGet('/api/users/1').willReturn(okJson({ id: 1, name: 'Alice' })))
);

await fetch(`${users.url}/api/users/1`);
```

`await using` closes the engine automatically at the end of scope (`Symbol.asyncDispose`); call
`await engine.close()` yourself if you're not in a context that supports it.

### Mountebank-compat `create()`

Existing Mountebank-style code — raw REST calls against the admin API, the `mb` CLI, or code
already written against the pre-0.15 `@rift-vs/rift` — keeps working unchanged via the default
export's `create()`:

```javascript
import rift from '@rift-vs/rift';

const server = await rift.create({ port: 2525, loglevel: 'info' });

// existing Mountebank-style REST calls / mb client code work unchanged against server.port
await fetch('http://localhost:2525/imposters', {
  method: 'POST',
  headers: { 'Content-Type': 'application/json' },
  body: JSON.stringify({
    port: 4545,
    protocol: 'http',
    stubs: [{
      predicates: [{ equals: { path: '/api/users' } }],
      responses: [{ is: { statusCode: 200, body: JSON.stringify([{ id: 1, name: 'Alice' }]) } }]
    }]
  })
});

const response = await fetch('http://localhost:4545/api/users');
console.log(await response.json()); // [{ id: 1, name: 'Alice' }]

await server.close();
```

`create()` is a permanent, first-class drop-in — not deprecated — so adopting the typed DSL above
is incremental, not a forced rewrite.

---

## Migration from Mountebank

Migrating from Mountebank is straightforward - just change the import:

**Before (Mountebank):**

```javascript
import mb from 'mountebank';

const server = await mb.create({
  port: 2525,
  loglevel: 'debug',
  allowInjection: true,
});
```

**After (Rift):**

```javascript
import rift from '@rift-vs/rift';

const server = await rift.create({
  port: 2525,
  loglevel: 'debug',
  allowInjection: true,
});
```

All your existing imposter configurations and test code work without changes.

---

## TypeScript Support

Full TypeScript definitions are included:

```typescript
import rift, { CreateOptions, RiftServer } from '@rift-vs/rift';

const options: CreateOptions = {
  port: 2525,
  loglevel: 'debug',
};

const server: RiftServer = await rift.create(options);

// Type-safe properties
console.log(server.port); // number
console.log(server.host); // string

await server.close();
```

---

## API Reference

### `create(options?: CreateOptions): Promise<RiftServer>`

Creates and starts a new Rift server instance.

#### CreateOptions

| Option           | Type       | Default       | Description                         |
|:-----------------|:-----------|:--------------|:------------------------------------|
| `port`           | `number`   | `2525`        | Admin API port                      |
| `host`           | `string`   | `'localhost'` | Bind address                        |
| `loglevel`       | `string`   | `'info'`      | Log level: debug, info, warn, error |
| `logfile`        | `string`   | -             | Path to log file                    |
| `datadir`        | `string`   | -             | Directory for imposter persistence (Mountebank `--datadir` parity) |
| `ipWhitelist`    | `string[]` | -             | Accepted, **not enforced** (see CLI reference) |
| `allowInjection` | `boolean`  | `false`       | Enable script injection             |

`impostersRepository` and `redis` (custom Mountebank repository config) are accepted by the type
for compatibility but rejected at runtime — Rift's native binary can't load a Node repository
module. Use `datadir` for persistence instead.

#### RiftServer

| Property/Method | Type                  | Description                    |
|:----------------|:----------------------|:-------------------------------|
| `port`          | `number`              | Port the server is listening on |
| `host`          | `string`              | Host the server is bound to    |
| `close()`       | `Promise<void>`       | Gracefully shutdown the server |

#### Events

The server emits the following events:

- `exit` - Emitted when the server process exits
- `error` - Emitted on server errors
- `stdout` - Emitted with stdout data
- `stderr` - Emitted with stderr data

---

## Testing Example with Jest

```javascript
import rift from '@rift-vs/rift';

describe('API Tests', () => {
  let server;

  beforeAll(async () => {
    server = await rift.create({ port: 2525 });

    // Create test imposters
    await fetch('http://localhost:2525/imposters', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        port: 4545,
        protocol: 'http',
        stubs: [{
          predicates: [{ equals: { method: 'GET', path: '/health' } }],
          responses: [{ is: { statusCode: 200, body: { status: 'ok' } } }]
        }]
      })
    });
  });

  afterAll(async () => {
    await server.close();
  });

  test('health check returns ok', async () => {
    const response = await fetch('http://localhost:4545/health');
    const data = await response.json();
    expect(data.status).toBe('ok');
  });
});
```

(`@rift-vs/rift/testkit/jest` also ships a `setupRift()` helper with automatic per-test imposter
teardown — see the SDK README's "Testkit" section.)

---

## Environment Variables

| Variable                       | Applies to    | Description                                |
|:--------------------------------|:-------------|:-------------------------------------------|
| `RIFT_BINARY_PATH`              | engine binary | Explicit path override; skips PATH/cache/download |
| `RIFT_FFI_LIB`                  | embedded cdylib | Explicit path override; skips cache/download (no checksum — you own the file) |
| `RIFT_CACHE_DIR`                | embedded cdylib | Overrides the cache root (defaults to `XDG_CACHE_HOME`, then `%LOCALAPPDATA%` on Windows, else `~/.cache`) |
| `RIFT_DOWNLOAD_URL`             | both          | Alternate release mirror base              |
| `RIFT_MIRROR_URL`               | engine binary | Alternate mirror base for the binary only (`RIFT_DOWNLOAD_URL` wins if both are set) |
| `RIFT_OFFLINE` / `RIFT_SKIP_BINARY_DOWNLOAD` | both | Air-gapped mode: never reach the network; resolution throws with manual-install instructions if nothing local is found |
| `RIFT_SKIP_CHECKSUM`            | engine binary only | Opt out of a missing (not mismatched) checksum sidecar — not available for the cdylib |

Note: these govern binary/library *resolution*, not installation — there's nothing to skip during
`npm install` since it never downloads anything (see "Installation" above).

---

## Manual Binary Installation

If resolution can't reach the network (e.g. behind a firewall) and nothing is cached or on `PATH`,
install the binary manually:

1. Download from [GitHub Releases](https://github.com/achird-labs/rift/releases)
2. Point the SDK at it:

```bash
export RIFT_BINARY_PATH=/path/to/rift
```

Or run `npx rift-fetch` ahead of time on a machine with network access, then copy its resolved
cache directory to the air-gapped one.

---

## SDK Development

This engine repository does not contain the Node.js SDK source — it only documents how to use the
published package. To build the SDK from source, run its tests, or contribute a change, see
[`achird-labs/rift-node`](https://github.com/achird-labs/rift-node) and its README/CONTRIBUTING
guide.

---

## Utility Functions

### `resolveBinary(options?): Promise<string>`

The current binary resolver. Resolution order:

1. `options.binaryPath` or `RIFT_BINARY_PATH`, if it exists on disk.
2. The first of `rift-http-proxy` / `rift` / `mb` found on `PATH` that `--version`-probes as Rift
   (a Mountebank `mb` shadowing the name is skipped).
3. A previously-downloaded copy in the local version cache.
4. Otherwise, download and checksum-verify the release archive for the resolved version (skipped
   entirely, throwing instead, when `RIFT_OFFLINE`/`RIFT_SKIP_BINARY_DOWNLOAD` is set).

### `findBinary(): Promise<string>` (deprecated)

Legacy wrapper over `resolveBinary()` limited to steps 1-3 above (no download); kept for
backward compatibility. Prefer `resolveBinary`.

### `downloadBinary(version?: string): Promise<string>` (deprecated)

Legacy wrapper that force-downloads a specific version, bypassing `PATH`/cache. Prefer
`resolveBinary({ version })`.

### `getBinaryVersion(): Promise<string | null>` (deprecated)

Returns the resolved binary's version string, or `null` if not found.

---

## Requirements

- **Node.js 20.0.0 or later.** The SDK uses the global `fetch`, `worker_threads`, and `await using`.
- **ESM only.** The package ships as ES modules with no CommonJS build — `import`, not `require()`.
- One of the supported platforms (see above)
