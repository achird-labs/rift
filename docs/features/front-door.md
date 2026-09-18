---
layout: default
title: Front Door
parent: Features
nav_order: 27
---

# Front Door

The [single-port gateway](gateway.md) lets one port reach every imposter, but the *client* has to
name the target (`/__rift/4545/api/users`). That is fine when the client is a test harness driving
Rift on purpose. It is wrong when the client is an unmodified system under test that believes it is
calling `payments.example.com`.

The front door closes that gap: **one listener, many imposters, addressed by what the request
says** — host, path prefix, header, method — with no client cooperation at all. It is the reason
you no longer need an nginx sidecar in front of Rift.

---

## Usage

```bash
rift --configfile mocks.json --front-door 0.0.0.0:8080
```

```json
{
  "imposters": [
    { "port": 4545, "protocol": "http", "stubs": [ ... ] },
    { "port": 4546, "protocol": "http", "stubs": [ ... ] }
  ],
  "routes": {
    "routes": [
      { "id": "payments", "match": { "host": "payments.test" }, "target": { "port": 4545 } },
      { "id": "search",   "match": { "host": "search.test"   }, "target": { "port": 4546 } }
    ]
  }
}
```

```bash
curl -H 'Host: payments.test' http://localhost:8080/api/charges   # -> imposter 4545
curl -H 'Host: search.test'   http://localhost:8080/api/query     # -> imposter 4546
```

The `routes` block is read only when `--front-door` is set; without the flag it is parsed and
validated but nothing listens for it. It may come from at most one imposter source (a second source
declaring `routes` is a startup error).

Dispatch is **in-process** — the same path the gateway uses. There is no second hop, no extra
socket, and the imposter behaves exactly as if the request had arrived on its own port.

---

## Route fields

| Field | Default | Meaning |
|---|---|---|
| `id` | — (required, non-empty) | Unique within the table; ties in evaluation order are broken on it. |
| `priority` | `0` | Higher is evaluated first. |
| `match` | `{}` | The clauses below. |
| `target` | — (required) | Where a matched request goes (see [Targets](#targets)). |
| `enabled` | `true` | A disabled route is kept in the table but never matches. |

---

## Matching

Every clause a route declares must match (they AND together). An empty `match` matches everything,
which is a legitimate catch-all.

| Clause | Meaning |
|---|---|
| `host` | Exact (`payments.test`), or one leading wildcard label (`*.payments.test`). Compared case-insensitively; any `:port` on the `Host` header is ignored. |
| `path_prefix` | **Segment-aligned**: `/api/v1` matches `/api/v1` and `/api/v1/users`, but never `/api/v1x`. A trailing `/` on the prefix is ignored. |
| `headers` | A list of `{ "name": ..., "value": ... }`. Names are case-insensitive; values must match byte for byte (only the first value of a repeated header is compared). |
| `method` | HTTP method, compared case-insensitively. |

A wildcard host means a real subdomain. `*.payments.test` matches `api.payments.test` but **not**
the bare `payments.test`, and **not** `evilpayments.test` — the `.` boundary is what makes a
wildcard a wildcard.

### Order is derived, not authored

Route tables get edited by several people and merged from several places, so "whichever was written
first wins" is a footgun. Instead the evaluation order is a total function of the routes themselves:

1. **`priority`**, descending — the explicit override (default `0`).
2. **Specificity** — an exact host beats a wildcard beats no host; a longer `path_prefix` beats a
   shorter one; more header clauses beat fewer.
3. **`id`**, ascending — unique, so there is never a tie left to break arbitrarily.

The same table therefore always resolves the same way, no matter what order it arrived in.

Two *enabled* routes whose `match` clauses are byte-identical at the same priority are an authoring
mistake with no right answer, so the whole table is rejected. Disabling one of them, or giving one
a different `priority`, is the supported way to stage a replacement.

---

## Targets

```json
{ "id": "api", "match": { "path_prefix": "/payments" },
  "target": { "port": 4545, "strip_prefix": true, "set_host": "internal.svc" } }
```

| Field | Default | Meaning |
|---|---|---|
| `port` | — | The imposter to dispatch to. It does **not** have to exist yet; routes and imposters can be deployed in either order. |
| `strip_prefix` | `false` | Remove the matched `path_prefix` before the imposter sees the path. Off by default, so predicates and recorded requests see the true path unless you ask otherwise. Query strings are always preserved. |
| `set_host` | — | Rewrite the `Host` the imposter sees. Rare, but recorded requests show it. |

---

## When nothing matches

The front door falls back to the gateway's own `/__rift/{port}/{path}` addressing, so one port
serves both styles and an empty route table behaves exactly like the gateway.

Past that, the response is `404` with a marker header:

```
x-rift-front-door: no-route
```

That header separates two failures that look identical from the client side and have completely
different fixes:

- **`404` with the header** — no route matched. Your route table is wrong.
- **`404` without it** — a route matched, but no imposter is listening on the port it names. Your
  imposter is missing.

---

## Validation

A route table is validated and applied **as a unit**, at load time — so a table that cannot route
fails the boot rather than the first request, and there is no half-applied routing topology to
reason about. A table is rejected when:

- a route has an empty `id`, or two routes share an `id`;
- two *enabled* routes at the same `priority` have byte-identical `match` clauses (see above);
- a route sets `strip_prefix` with no `path_prefix` to strip;
- a `host` contains `*` anywhere but as one leading `*.` label;
- a `path_prefix` does not start with `/`, or a `method` is not a valid HTTP method.

Every message names the offending route id, because "invalid route table" and a 400 is not
actionable.

A `routes` block is only read from the `{"imposters": [...], "routes": {...}}` wrapper form. Putting
one on a single-imposter document is an error rather than a silent no-op — unknown fields are
otherwise ignored, so the quiet version would be no routes, no diagnostic, and a green boot.

`POST /admin/reload` re-applies an edited `routes` block, after the reloaded imposters, as a
restart would. An invalid table refuses the reload and leaves the old table serving. Without
`--front-door`, a `routes` block is ignored, and both the startup log and the reload response's
`warnings` say so. See [Hot Reload](hot-reload.md).

---

## Observing dispatches (embedders)

An embedder can count traffic per route by passing a `RouteObserver` to
`bind_front_door_with_observer`. Its `note_dispatch(route_id)` is called once for every request a
route claims, including one whose imposter is gone and answers `404`. It is not called for the
`/__rift/{port}` fallback or for the `no-route` 404. `bind_front_door` is the same call with no
observer.
