# Example Imposter Configurations

Six ready-to-run configs, each a single self-contained Mountebank-format file. They are checked by
`rift-lint` in CI, so they parse and load.

| File | Port | What it shows |
|:-----|:-----|:--------------|
| [`basic-api.json`](basic-api.json) | 4545 | The smallest useful config — a health check and CRUD over `/api/users`. Start here. |
| [`task-management-api.json`](task-management-api.json) | 4545 | A fuller REST surface: regex path predicates, path parameters, and per-resource responses. |
| [`authentication-api.json`](authentication-api.json) | 4547 | Login / logout / token validation, using `scenarioName` so a session advances through states. |
| [`feature-flags-api.json`](feature-flags-api.json) | 4546 | Flag lookup with a catch-all regex for unknown keys. |
| [`error-testing.json`](error-testing.json) | 4545 | Every error status your client should handle — 400, 401, 403, 404, 429, 500, 502, 503. |
| [`latency-testing.json`](latency-testing.json) | 4545 | Fixed and random delays plus a timeout endpoint, for exercising client retry and timeout logic. |

Several use port 4545, so run one at a time unless you edit the ports.

## Running one

```bash
# With a locally installed binary
rift --configfile examples/basic-api.json

# With Docker
docker run -p 2525:2525 -p 4545:4545 \
  -v "$(pwd)/examples:/examples" \
  zainalpour/rift-proxy:latest --configfile /examples/basic-api.json
```

Then drive it:

```bash
curl http://localhost:4545/health
curl http://localhost:4545/api/users
```

## Validating before you load

```bash
rift-lint examples/
```

## Where to go next

These cover the Mountebank-compatible surface. For Rift's own features — fault injection, scripting,
scenarios, flow state, the front door, the TLS intercept proxy — see the runnable `docker-compose`
demos in [`../docs/demo/`](../docs/demo/), each of which comes up in one command, and the
[Features documentation](https://achird-labs.github.io/rift/features/).
