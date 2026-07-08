---
layout: default
title: Correlated Isolation (Spaces)
parent: Features
nav_order: 21
---

# Correlated Isolation (Spaces)

A **space** partitions one imposter's stubs and state by a correlation id (the *flow id*), so
parallel test runs sharing a port don't see each other's stubs, scenario state, or recorded
requests.

---

## How a request's flow id is resolved

`_rift.flowState.flowIdSource` decides which flow (space) a request belongs to:

| `flowIdSource` | Resolution |
|:---------------|:-----------|
| `"imposter_port"` (default) | The imposter port — one shared space. |
| `"header:X-Mock-Space"` | The value of that request header (case-insensitive); falls back to the port if the header is absent. |

A stub's optional `space` field scopes it to one flow id. Stubs **without** a `space` are global and
match any caller. Space-scoped stubs are considered only when the request's resolved flow id equals
their `space`.

---

## Example — one port, isolated tenants

```json
{
  "port": 4510,
  "protocol": "http",
  "recordRequests": true,
  "_rift": {
    "flowState": {
      "backend": "inmemory",
      "ttlSeconds": 300,
      "flowIdSource": "header:X-Mock-Space"
    }
  },
  "stubs": [
    {
      "space": "alice",
      "predicates": [{ "equals": { "path": "/data" } }],
      "responses": [{ "is": { "statusCode": 200, "body": { "owner": "alice" } } }]
    },
    {
      "space": "bob",
      "predicates": [{ "equals": { "path": "/data" } }],
      "responses": [{ "is": { "statusCode": 200, "body": { "owner": "bob" } } }]
    },
    {
      "predicates": [{ "equals": { "path": "/health" } }],
      "responses": [{ "is": { "statusCode": 200, "body": "OK" } }]
    }
  ]
}
```

```bash
curl -H 'X-Mock-Space: alice' http://localhost:4510/data   # {"owner":"alice"}
curl -H 'X-Mock-Space: bob'   http://localhost:4510/data   # {"owner":"bob"}
curl http://localhost:4510/health                          # OK  (global stub)
```

---

## Recorded requests, scoped to a space

With `recordRequests: true`, `GET /imposters/{port}/savedRequests` accepts a `match=flow_id=<value>`
query parameter (see the [API Reference]({{ site.baseurl }}/api/#requests)) to read or clear only
one space's requests, leaving other spaces and the imposter itself untouched:

```bash
# Read only alice's recorded requests
curl 'http://localhost:2525/imposters/4510/savedRequests?match=flow_id=alice'

# Clear only alice's recorded requests (bob's and the port's own are unaffected)
curl -X DELETE 'http://localhost:2525/imposters/4510/savedRequests?match=flow_id=alice'
```

---

## Managing spaces at runtime

Instead of declaring `space` inline, you can add stubs to a space through the admin API and tear the
whole space down in one call (its scoped stubs, recorded requests, and scenario state):

```bash
curl -X POST http://localhost:2525/imposters/4510/spaces/alice/stubs \
  -d '{"predicates":[{"equals":{"path":"/data"}}],"responses":[{"is":{"statusCode":200,"body":"scoped"}}]}'

curl http://localhost:2525/imposters/4510/spaces/alice        # inspect the space
curl -X DELETE http://localhost:2525/imposters/4510/spaces/alice   # teardown
```

Spaces are addressed by a known flow id — there is no bare `GET /imposters/{port}/spaces` route to
list or discover which spaces currently exist under an imposter. If you need an inventory of active
flow ids, track them on the caller side (or derive them from recorded requests / stub `space`
fields); the admin API only ever answers "what does *this* flow id look like."

Spaces build on the same store as [Flow State]({{ site.baseurl }}/features/flow-state/) and
[Scenarios]({{ site.baseurl }}/features/scenarios/), which are likewise partitioned by flow id.

---

## Embedding over the C-ABI

Everything above is also reachable with zero loopback HTTP from an embedded Rift: the FFI exposes
`rift_space_add_stub`, `rift_space_list_stubs`, `rift_space_delete`, and `rift_space_recorded`,
each mirroring the corresponding admin-HTTP handler exactly. See
[FFI (C-ABI)]({{ site.baseurl }}/embedding/ffi/) for signatures and ownership rules.
