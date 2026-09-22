---
layout: default
title: API Reference
nav_order: 7
permalink: /api/
---

# REST API Reference

Rift provides a Mountebank-compatible REST API for managing imposters.

---

## Base URL

```
http://localhost:2525
```

---

## Authentication

When Rift is started with `--api-key <TOKEN>` (or `MB_APIKEY`), every admin API request must send the
token in the `Authorization` header. Requests without a matching token receive `401 Unauthorized`.
Data-plane traffic — direct imposter ports and the `/__rift/:port/...` gateway — is not gated.

A blank token is a startup error, not a key: it would enable this gate and then authenticate every
request. Omit the option to run the admin API explicitly unauthenticated.

```bash
curl -H "Authorization: <TOKEN>" http://localhost:2525/imposters
```

---

## Root

### GET /

Get API information and links.

**Response:**
```json
{
  "_links": {
    "imposters": { "href": "/imposters" },
    "config": { "href": "/config" },
    "logs": { "href": "/logs" }
  }
}
```

---

## Imposters

### GET /imposters

List all imposters.

Imposters are returned in **ascending port order**. This ordering is guaranteed and stable across
calls, so a client may rely on it — for example when diffing two snapshots of the server state.
Earlier releases followed an internal hash map order, which could vary between calls (#713).

**Query Parameters:**
- `replayable` (boolean) - Return each imposter's full config, for export
- `removeProxies` (boolean, with `replayable`) - Strip proxy responses from the export
- `list` (boolean) - Return a shorter entry per imposter: `protocol`, `port`, `name`,
  `numberOfRequests`, `_links`

Without either flag each entry is the summary shown below, plus `_links`.

**Response:**
```json
{
  "imposters": [
    {
      "port": 4545,
      "protocol": "http",
      "name": "User Service",
      "numberOfRequests": 42,
      "stubCount": 3,
      "enabled": true,
      "recordRequests": false
    },
    {
      "port": 4546,
      "protocol": "https",
      "name": "Payment Service",
      "numberOfRequests": 15,
      "stubCount": 1,
      "enabled": true,
      "recordRequests": true
    }
  ]
}
```

**Example:**
```bash
curl http://localhost:2525/imposters
curl "http://localhost:2525/imposters?replayable=true"
```

---

### POST /imposters

Create a new imposter.

**Request Body:**
```json
{
  "port": 4545,
  "protocol": "http",
  "name": "My Service",
  "stubs": [
    {
      "predicates": [{ "equals": { "path": "/test" } }],
      "responses": [{ "is": { "statusCode": 200, "body": "OK" } }]
    }
  ]
}
```

**Response:** `201 Created` with the imposter detail (as `GET /imposters/{port}`). `400` for invalid
JSON or an invalid imposter. That includes a single-valued header object (`proxy.injectHeaders`,
`_rift.fault.error.headers`) naming one header twice in different cases (#1050), and a scripted
config without `--allowInjection` (`400 invalid injection`).
```json
{
  "port": 4545,
  "protocol": "http",
  "name": "My Service",
  "numberOfRequests": 0,
  "stubs": [...]
}
```

**Example:**
```bash
curl -X POST http://localhost:2525/imposters \
  -H "Content-Type: application/json" \
  -d '{
    "port": 4545,
    "protocol": "http",
    "stubs": [{
      "responses": [{ "is": { "statusCode": 200 } }]
    }]
  }'
```

---

### PUT /imposters

Replace all imposters (bulk create/update). The running set is *reconciled* toward the payload —
the same incremental engine as `POST /admin/reload` — rather than deleted wholesale and recreated:
imposters absent from the payload are deleted, changed ones are replaced (or stub-patched), and an
imposter whose config is unchanged keeps its runtime state (recorded requests, response cycling).
The whole set is validated before anything is touched, so an invalid payload never disturbs the
running imposters. Use `DELETE /imposters` first if you also want unchanged imposters reset.

**Request Body:**
```json
{
  "imposters": [
    { "port": 4545, "protocol": "http", "stubs": [...] },
    { "port": 4546, "protocol": "http", "stubs": [...] }
  ]
}
```

**Response:** `200 OK`
```json
{
  "imposters": [...]
}
```

**Errors:**
- `400 Bad Request` — the set failed validation (bad protocol, duplicate port, duplicate stub id);
  the running imposters are unchanged. An absent or `0` port is auto-assigned, never a duplicate;
  such an imposter is re-created on each `PUT`, after every imposter with an explicit port, so it
  never takes a port an explicit imposter in the set is serving.
- `500 Internal Server Error` — one or more imposters failed to apply (e.g. a port bind failure);
  the body carries the per-port `failed` list plus the `created`/`replaced`/`stubPatched`/`deleted`
  report of what did apply, mirroring `POST /admin/reload`.

---

### GET /imposters/{port}

Get imposter details.

**Query Parameters:**
- `replayable` (boolean) - Include full configuration for export
- `removeProxies` (boolean) - Exclude proxy stubs

**Response:**
```json
{
  "port": 4545,
  "protocol": "http",
  "name": "My Service",
  "numberOfRequests": 42,
  "requests": [
    {
      "method": "GET",
      "path": "/test",
      "headers": {...},
      "timestamp": "2024-01-15T10:30:00.000Z"
    }
  ],
  "stubs": [...]
}
```

**Example:**
```bash
curl http://localhost:2525/imposters/4545
curl "http://localhost:2525/imposters/4545?replayable=true"
```

---

### DELETE /imposters/{port}

Delete an imposter.

The response is returned only **after the imposter is fully torn down** (issue #596): its listener
socket is unbound and its established (keep-alive) connections are closed — bounded by a short drain
for any in-flight response. So once `DELETE` returns you can immediately re-`POST` an imposter on the
same port without racing the old one: a pooled client connection gets a clean close and reconnects to
the new imposter, never the deleted one's state.

The handler reads no query parameters.

With `--datadir`, the imposter's `<datadir>/<port>.json` is removed first. If that fails, the call
returns `503` naming the file and the imposter is **not** deleted: it keeps serving, and deleting it
anyway would bring it back on the next restart.

**Response:** `200 OK` with a snapshot of the deleted imposter (`numberOfRequests` `0`, `requests`
empty); `404` if no imposter is on that port.
```json
{
  "port": 4545,
  "protocol": "http",
  "stubs": [...]
}
```

**Example:**
```bash
curl -X DELETE http://localhost:2525/imposters/4545
```

---

### DELETE /imposters

Delete all imposters.

If an imposter's `<datadir>/<port>.json` cannot be removed, that imposter is not deleted and keeps
serving. The call then returns `503` with an `errors` entry naming each such port and file, and
`imposters` lists the ones that were deleted.

**Response:** `200 OK`
```json
{
  "imposters": [...]
}
```

**Example:**
```bash
curl -X DELETE http://localhost:2525/imposters
```

---

## Stub Management

### GET /imposters/{port}/stubs

List all stubs for an imposter: `{"stubs": [...]}`, each stub with its HATEOAS `_links`. `404` if
there is no imposter on that port.

---

### PUT /imposters/{port}/stubs

Replace every stub on the imposter. **Request Body:** `{"stubs": [ ... ]}`. **Response:** `200 OK`
with the imposter detail. `400` for invalid JSON, an invalid stub, or a scripted stub without
`--allowInjection`.

---

### POST /imposters/{port}/stubs

Add a stub to an existing imposter.

**Request Body:**
```json
{
  "stub": {
    "predicates": [{ "equals": { "path": "/new" } }],
    "responses": [{ "is": { "statusCode": 200 } }]
  },
  "index": 0
}
```

`index` is optional (it appends when omitted); an out-of-range index is a `400`.

**Response:** `200 OK` with the imposter detail.

**Example:**
```bash
curl -X POST http://localhost:2525/imposters/4545/stubs \
  -H "Content-Type: application/json" \
  -d '{
    "stub": {
      "predicates": [{ "equals": { "path": "/new" } }],
      "responses": [{ "is": { "statusCode": 201 } }]
    }
  }'
```

---

### GET /imposters/{port}/stubs/{index}

Get a single stub by its array index, with its `_links`.

---

### PUT /imposters/{port}/stubs/{index}

Replace a stub at a specific index. The body is the bare stub, with no `{"stub": …}` envelope.
**Response:** `200 OK` with the imposter detail.

**Request Body:**
```json
{
  "predicates": [{ "equals": { "path": "/updated" } }],
  "responses": [{ "is": { "statusCode": 200 } }]
}
```

---

### DELETE /imposters/{port}/stubs/{index}

Delete a stub at a specific index. **Response:** `200 OK` with the imposter detail.

---

### Stub operations by stable id

Every stub has a stable `id` (auto-generated as a UUID when omitted). These endpoints address a stub
by that id instead of by positional index, so concurrent edits don't shift the target.

| Method | Path | Action |
|:-------|:-----|:-------|
| `GET` | `/imposters/{port}/stubs/by-id/{id}` | Get the stub with this id (the bare stub JSON) |
| `PUT` | `/imposters/{port}/stubs/by-id/{id}` | Replace the stub with this id (position preserved) |
| `DELETE` | `/imposters/{port}/stubs/by-id/{id}` | Delete the stub with this id |

`PUT` takes the bare stub. `PUT` and `DELETE` answer `200` with the imposter detail. An unknown id
is a `404`.

```bash
curl http://localhost:2525/imposters/4545/stubs/by-id/6f1c...e2
```

---

## Imposter State

### POST /imposters/{port}/enable

Re-enable a disabled imposter.

### POST /imposters/{port}/disable

Disable an imposter — it stops matching stubs and returns a default response — without deleting it.

Both answer `200` with `{"message": "Imposter enabled"}` / `{"message": "Imposter disabled"}`.

```bash
curl -X POST http://localhost:2525/imposters/4545/disable
curl -X POST http://localhost:2525/imposters/4545/enable
```

---

## Requests

### GET /imposters/{port}/savedRequests

Get recorded requests (if `recordRequests: true`). Also available under the alias
`GET /imposters/{port}/requests`.

**Query Parameters:**
- `match=header:<Name>=<Value>` — keep only requests carrying a matching header
- `match=flow_id=<Value>` — keep only requests whose resolved flow id matches
- `match=method=<Verb>` — keep only requests whose method matches exactly (case-sensitive)
- `match=path=<Path>` — keep only requests whose bare path matches exactly (the query string is not compared)
- `since=<index>` — keep only requests newer than a cursor (see [Tailing with a cursor](#tailing-with-a-cursor))

Multiple `match` clauses are AND-ed together. `since` is applied first, then the `match` clauses.

**Response:** a JSON array of recorded requests. Each element carries `requestFrom` (the client
`ip:port`); `body` is present only when the request had one. `status` and `latencyMs` (issue #940)
give the status sent back and how long the imposter took to produce it, in whole milliseconds. They
are either both present or both absent. Absent means "not recorded", never `0`: the `X-Rift-Debug`
path, a request that errored before responding, and a custom journal without stable indices all
leave them out. `latencyMs: 0` is a normal reading for a stub served from memory. `node` is present
only when a clustered embedder's journal sets it; single-node Rift never does.
```json
[
  {
    "requestFrom": "127.0.0.1:52344",
    "method": "GET",
    "path": "/api/users",
    "query": {},
    "headers": {
      "host": "localhost:4545",
      "user-agent": "curl/7.88.0"
    },
    "timestamp": "2024-01-15T10:30:00.000Z",
    "status": 404,
    "latencyMs": 0,
    "matchOutcome": {
      "matched": false,
      "tried": [
        { "stubIndex": 0, "stubId": "users",
          "why": { "reason": "failedPredicate", "predicateIndex": 1 } },
        { "stubIndex": 1, "why": { "reason": "skippedScenarioState" } }
      ]
    }
  }
]
```

#### Why a request did not match

`matchOutcome` answers that without re-deriving the scan by hand. On a hit it carries
`matched: true` plus `stubIndex`/`stubId`; on a miss, `matched: false` and no winner.

`tried` lists the candidates the matcher **visited**, in visit order — a stub the candidate index
ruled out before the scan, or one sitting after the winner, is not listed, because no verdict was
ever reached for it. Each entry says why that candidate fell out: `failedPredicate` with the
position of the first predicate the request failed (predicates are AND-ed and the scan
short-circuits, so later ones were never evaluated), or `skippedSpace` / `skippedScenarioState`
for a stub whose eligibility gate excluded it before predicates ran at all.

The list is capped at 25 entries; anything beyond that is counted in `triedOmitted` rather than
silently dropped.

The whole object is **absent** when no outcome was recorded — `recordRequests` was off (in which
case there is no entry at all), a custom `RequestJournal` backend has no stable indices to attach
to, the request took the `X-Rift-Debug` path, or matching itself errored. Absence means "not
recorded", never "did not match".

#### Tailing with a cursor

Polling this endpoint without a cursor re-sends the whole journal every time. `since=<index>`
makes a poll cost only what is new. Every recorded request is assigned a **stable, 1-based,
per-port index**; the cursor rides in response headers so the body above is unchanged.

**Response headers:**

| Header | Meaning |
|---|---|
| `x-rift-next-index` | The cursor to pass as the next `since`. `0` means nothing has been recorded yet. |
| `x-rift-truncated` | Present (`true`) only when retention discarded entries you had not seen — your view has a hole. Absent otherwise; it is never `false`. |

```bash
# Baseline: everything retained, plus the cursor to resume from.
curl -i localhost:2525/imposters/3000/savedRequests
# x-rift-next-index: 12

# Only what arrived since — composes with match=.
curl -i "localhost:2525/imposters/3000/savedRequests?since=12&match=flow_id=tenant-a"
```

**Contract:**

- **`since` is exclusive** — you receive entries strictly newer than the index you pass. Pass
  back `x-rift-next-index` verbatim; a cursor at or beyond the tip returns an empty array.
- **`x-rift-next-index` always advances past everything scanned**, including entries your
  `match=` clauses rejected. A filtered tail therefore never re-scans the same range.
- **Indices survive deletion.** `DELETE savedRequests` and scoped clears do not reset them:
  entries recorded afterwards simply get larger indices, so a cursor held across a clear stays
  valid and is *not* reported as truncated. Deleting data you asked to delete is not a hole.
- **`x-rift-truncated` means one thing:** the 10,000-entry cap evicted entries you had not seen.
  Re-poll without `since` to rebuild a baseline. Note that `since=0` ("replay everything") does
  report truncation once anything has been evicted, while omitting `since` ("snapshot what is
  retained") never does — the two return the same entries but ask different questions.
- **No `x-rift-next-index` means do not advance.** Keep your existing cursor and poll again.
  Its absence covers three cases, all handled the same way: an older engine (which ignores the
  unknown parameter), a custom `RequestJournal` backend without stable indices, and a backend
  that served a *degraded* partial read. In the degraded case the entries returned are real but
  incomplete, so the cursor is deliberately withheld — advancing on it would skip the entries the
  backend could not reach. A synthetic index is never returned, because offsets shift under
  eviction and would silently skip or replay entries.

**Canonical SDK tail:** baseline poll → keep `x-rift-next-index` → poll `?since=<cursor>` on an
interval, updating the cursor each time → on `x-rift-truncated`, re-baseline.

---

### POST /imposters/{port}/verify

Count — and optionally return — recorded requests matching a predicate set, evaluated by the
engine's own predicate engine rather than re-implemented per client. This is what an SDK's
`verify(match, times(n))` calls instead of fetching `savedRequests` and re-evaluating predicates
locally (where operators like `xpath`/`inject` are impractical) or shipping the whole journal over
the wire just to count it.

**Request body:**
```json
{
  "predicates": [ { "equals": { "path": "/api/users" } } ],
  "flowId": "tenant-a",
  "includeRequests": false,
  "includeClosest": false
}
```
- `predicates` — standard Mountebank/Rift predicate objects, AND-ed together (same semantics as a
  stub's `predicates`). A request header the client sent more than once matches if **any** of its
  values satisfies the predicate — and `not` on such a header fails as soon as one value matches.
  That is the same rule live stub matching, intercept rule matching and `savedRequests` filtering
  all apply (#1025, #1026), so a `verify` count and what actually matched at request time agree.
  Header names are compared case-insensitively, and a recorded request's `headers` holds one entry
  per name however the document spelled it (#1039). Note that this normalisation applies to the
  *request* side only: a `deepEquals` object in your own predicate is compared as written, so
  spelling one name twice there still counts as two names and will not match.
- `flowId` *(optional)* — scope the count to one space, resolved via the imposter's
  `flow_id_source` (the same scoping as `match=flow_id=<Value>` on `savedRequests`).
- `includeRequests` *(optional, default `false`)* — return the matching requests, not just the count.
- `includeClosest` *(optional, default `false`)* — return the best-scoring non-match — the request
  satisfying the most predicate clauses (ties resolve to the most recent) — with per-clause failure
  details, for rendering a readable diff on a failed verification.

An `inject` predicate requires the server to be started with `--allowInjection`; otherwise the
request is rejected with `400 invalid injection` (the same gate the stub endpoints apply).

**Response:**
```json
{
  "matched": 2,
  "total": 17,
  "requests": [ /* present only with includeRequests */ ],
  "closest": {
    "request": { /* the closest non-matching recorded request */ },
    "failedPredicates": [
      { "predicate": { "equals": { "path": "/api/users" } }, "actual": { "path": "/api/orders" } }
    ]
  }
}
```
`matched` counts requests matching every predicate; `total` is the number of recorded requests in
scope (after any `flowId` filter). `requests`/`closest` are present only when the corresponding
option is set.

---

### DELETE /imposters/{port}/savedRequests

Clear recorded requests. Also available under the alias `DELETE /imposters/{port}/requests`.
Accepts the same `match=` query parameters as the `GET`, in which case only matching requests are
removed.

---

### DELETE /imposters/{port}/savedProxyResponses

Clear responses recorded by proxy stubs (`proxyOnce` / `proxyAlways`), leaving the imposter's other
state intact.

---

## Events (Server-Sent Events)

### GET /events

A [Server-Sent Events](https://developer.mozilla.org/docs/Web/API/Server-sent_events) stream of
recorded requests and imposter lifecycle changes — a push upgrade of polling `GET /savedRequests`,
for live request tails (`ZStream`/`fs2.Stream`, Go channels, async iterators). Gated by the admin
API key like every other admin route. Older engines return `404`, so an SDK probes this endpoint and
falls back to polling.

**Query parameters:**
- `types=requests,lifecycle` — which event families to stream (default: both).
- `port=<port>` — restrict to one imposter.
- `match=header:<Name>=<Value>` / `match=flow_id=<Value>` / `match=method=<Verb>` /
  `match=path=<Path>` — filter **request** events (AND-ed). `method=`/`path=` are exact-equality
  against the recorded request. `flow_id=` compares the request's record-time resolved flow id (per
  the imposter's `flow_id_source`); a `header:`-source imposter whose request lacks that header falls
  back to the port, which `GET /savedRequests?match=flow_id=` treats as "no match" instead — the only
  edge where the two disagree.

**Event stream** (`Content-Type: text/event-stream`):
```
event: hello
data: {"engineVersion":"X.Y.Z","seq":42,"types":["requests","lifecycle"],"port":null}

event: request
id: 43
data: {"port":3000,"flowId":"tenant-a","index":12,"request":{ …RecordedRequest, as recorded… }}

event: imposter
id: 44
data: {"action":"created|replaced|stubsChanged|deleted|allDeleted","port":3000}

event: lagged
data: {"missed":7}

: ping    ← comment heartbeat every 15s
```

- **Request events require `recordRequests: true`** — the stream is a tail *of recorded requests*,
  exactly like `savedRequests`, not a tap of all traffic.
- A request event is pushed **when the request is recorded**, before it is matched or answered, so
  its `request` never carries `matchOutcome`, `status` or `latencyMs`. Fetch the entry from
  `GET /savedRequests?since=<index − 1>` when you need them.
- The `id:` is a monotonic sequence number spanning **both** event families. **v1 does not replay:**
  on reconnect, a gap in `id:` (or a `lagged` event, emitted when a slow consumer falls behind the
  bounded buffer) means "reconcile via `GET /savedRequests`". The stream is lossy-but-loud by
  design; polling remains the source of truth.
- **`index`** on a request event is that entry's journal index — the same cursor the polling side
  reports as `x-rift-next-index` (see [Tailing with a cursor](#tailing-with-a-cursor)). It is what
  makes reconciling cheap: pass the last `index` you saw as `?since=<index>` and get only what you
  missed, instead of re-polling the whole journal and de-duplicating by content. Omitted when the
  journal backend has no stable indices — the same capability probe as the polling side's missing
  header.

**Canonical tail:** connect → `hello` → baseline `GET /savedRequests` (keep `x-rift-next-index`) →
consume events, tracking `index` → on `lagged` or a reconnect gap, `GET /savedRequests?since=<last
index>` to fill the hole, then resume.

### GET /imposters/{port}/savedRequests/stream

Sugar alias for `GET /events?types=requests&port={port}` — a handle-scoped request tail that mirrors
the `savedRequests` polling endpoint one-to-one.

---

## Scenarios

Declarative state machines (Mountebank/WireMock style) gate stubs by `requiredScenarioState` and
transition via `newScenarioState`. State is partitioned per flow id.

### GET /imposters/{port}/scenarios

List scenario states. Accepts an optional `?flowId=<id>` query parameter (defaults to the imposter port).
**Response:** `{"flowId", "scenarios": [{"name", "state"}]}`.

### PUT /imposters/{port}/scenarios/{name}/state

Arrange a scenario's state directly.

**Request Body:** `{ "state": "AWAITING_PAYMENT", "flowId": "order-42" }` (`flowId` optional; a
missing `state` is a `400`). **Response:** `{"flowId", "name", "state"}`.

### POST /imposters/{port}/scenarios/reset

Reset scenarios. **Request Body:** `{ "flowId": "order-42" }` (optional; omit to reset the default flow).
**Response:** `{"flowId", "reset": true}`.

---

## Spaces (Correlated Isolation)

A "space" isolates stubs and state to a correlation id (`flowId`), so parallel test runs don't collide.

### POST /imposters/{port}/spaces/{flowId}/stubs

Add a stub scoped to this space. The body is the **bare stub**. This differs from
`POST /imposters/{port}/stubs`, which takes a `{"stub": …}` envelope. **Response:** `201 Created`
with `{"space", "stubs"}`.

A body that has none of the recognised stub fields is refused with `400` (#932). Before that fix it
created a stub with no predicates, which matched everything in the space. A body with a `stub` key
gets a message that points to the envelope mistake. `{}` and `{"predicates": []}` are still
accepted as a space-wide default.

### GET /imposters/{port}/spaces/{flowId}/stubs

List this space's stubs: `{"space", "stubs"}`.

### GET /imposters/{port}/spaces/{flowId}

Inspect the space: `{"space", "stubs", "scenarios": [{"name", "state"}], "numberOfRequests"}`.

### DELETE /imposters/{port}/spaces/{flowId}

Tear down the space, removing its scoped stubs, recorded requests, and scenario state.
**Response:** `{"space", "tornDown": true}`.

---

## Flow State

A per-flow key/value store backing stateful stubs (e.g. retry-then-succeed). These admin endpoints
inspect and arrange it directly.

| Method | Path | Action |
|:-------|:-----|:-------|
| `GET` | `/admin/imposters/{port}/flow-state/{flow_id}/{key}` | Read a value: `{"flowId","key","value"}`, or `404` if absent |
| `PUT` | `/admin/imposters/{port}/flow-state/{flow_id}/{key}` | Set a value. Body `{ "value": <any JSON> }` (a missing `value` is a `400`); returns `{"flowId","key","value"}` |
| `DELETE` | `/admin/imposters/{port}/flow-state/{flow_id}/{key}` | Delete a key: `{"flowId","key","deleted":true}` |
| `DELETE` | `/admin/imposters/{port}/flow-state/{flow_id}` | Delete every key under `flow_id` (issue #530). Idempotent: `{"flowId","cleared":true}` |

Each route answers `404` for an unknown imposter, and returns the backend-unavailable `503` when
the flow store fails.

---

## Intercept proxy

These routes exist only when the server was built with an intercept control. The `rift` binary
always has one, and so does an embedded `rift_serve_admin`. Without one, every `/intercept*` path
returns `404`. The request and rule schemas, CA options and authentication are documented in
[Intercept proxy]({{ site.baseurl }}/features/intercept-proxy/#runtime-lifecycle-admin-api).
Authorization actions are `intercept.read` / `intercept.write`.

| Method | Path | Success | Errors |
|:-------|:-----|:--------|:-------|
| `POST` | `/intercept` | `201` + `{"interceptPort","interceptUrl"}` (plus `caCertPem`/`caKeyPem` with `returnCaKey`). An empty body or `{}` means defaults. The body may seed `rules`. | `400` bad options, CA or bind; `400 invalid injection` for a scripted seeded rule without `--allowInjection`; `403` off-host with no `auth` under `--require-admin-auth`; `409` already running; `429` seeded rules over capacity |
| `GET` | `/intercept` | `200` + `{"interceptPort","interceptUrl"}` | `404` not running |
| `DELETE` | `/intercept` | `204`, idempotent. Drops rules and the CA. | — |
| `POST` | `/intercept/rules` | `201` + the added rules as an array. The body is one rule object or an array of rules. | `400` bad JSON or scripted rule; `404` not running; `429` rule store full |
| `GET` | `/intercept/rules` | `200` + array of rules | `404` not running |
| `DELETE` | `/intercept/rules` | `200` + `{"deleted": N}` | `404` not running |
| `GET` | `/intercept/ca.pem` | `200`, `application/x-pem-file` | `404` not running |
| `GET` | `/intercept/truststore.p12`, `/intercept/truststore.jks` | `200`, binary truststore (`?password=`, default `changeit`) | `404` not running; `500` export failure |

A serve rule's `body` may be any JSON value (#934), and a serve rule accepts the Mountebank
`statusCode` and `headers` spellings (#938). The
[rule schema]({{ site.baseurl }}/features/intercept-proxy/#configuring-rules-admin-api) is canonical.

---

## Gateway

### /__rift/{port}/&lt;path&gt;

Dispatch any request to the imposter on `{port}`, rewriting the URI to `/<path>`. Lets a
containerized Rift publish only the admin port while still reaching every imposter. Works with any
HTTP method and is not gated by `--api-key`.

```bash
# equivalent to hitting the imposter on port 4545 at /api/users
curl http://localhost:2525/__rift/4545/api/users
```

---

## System

### GET /health

Liveness check. Returns `{"status":"ok"}`.

### GET /metrics

Prometheus-format metrics (imposter count, per-imposter request counts). Also exposed on the
dedicated metrics port (`--metrics-port`, default 9090).

### POST /admin/reload

Hot-reload imposters from the startup config source (`--configfile` / `--datadir`), applying the
difference incrementally. A no-op (200) when no config source was provided. New config is
validated before any running imposter is changed. With both `--configfile` (or `--imposters`) and
`--datadir`, both are re-read and applied as one set. See [Hot Reload](../features/hot-reload.md).

---

## Configuration

### GET /config

Get current configuration.

**Response:**
```json
{
  "version": "X.Y.Z",
  "commit": "<sha>",
  "options": {
    "port": 2525,
    "allowInjection": true,
    "localOnly": false,
    "ipWhitelist": ["*"]
  },
  "serveOptions": [
    "host", "port", "apiKey", "metricsPort", "configFile", "noParse", "config",
    "allowInjection", "requireAdminAuth", "upstreamCaFile", "upstreamCaPem",
    "upstreamTlsSkipVerify"
  ],
  "process": {
    "nodeVersion": "N/A (Rust)", "architecture": "aarch64", "platform": "macos",
    "rss": 0, "heapTotal": 0, "heapUsed": 0, "uptime": 0, "cwd": "/srv/rift"
  }
}
```

`commit` is `null` unless the build was stamped with one. `process` mirrors Mountebank's shape: only
`architecture`, `platform` and `cwd` carry real values.

Field notes (issue #879 — these were previously hardcoded literals):

- **`port`** is the port the admin plane actually bound, so a `--port 0` (ephemeral) server reports
  the real one rather than the `2525` default. An embedder that fronts the admin API with its own
  public listener can override it with `AdminApiServer::with_reported_admin_port` /
  `ServerBuilder::reported_admin_port` (issue #1135), so clients building URLs from `options.port`
  get the public port. See [Embeddable Server]({{ site.baseurl }}/embedding/server/).
- **`localOnly`** reports whether **`--local-only` was supplied**, not whether the admin listener
  happened to bind loopback. The distinction matters: `--host 127.0.0.1` narrows only the admin
  plane, while `/metrics` and every imposter port stay on `0.0.0.0` — so reporting `true` there
  would tell you nothing is reachable off-host while two listener families still are.
- **`ipWhitelist`** is always `["*"]`. `--ip-whitelist` is accepted for Mountebank compatibility and
  **never enforced**, so every address may connect; see the
  [CLI reference]({{ site.baseurl }}/configuration/cli/#--ip-whitelist-does-not-filter-anything).

`serveOptions` (since 0.17.0, issue #877; 0.17.0 listed the first eight keys without `noParse`)
lists the keys the embedded serve-options document accepts, so a consumer can feature-detect an option before sending it. **Absence of the key means an
engine too old to report capabilities** — treat every option as unsupported rather than assuming a
rejection you will never receive. It is the same list `rift_build_info().serveOptions` publishes over
the C-ABI, and is a sibling of `options` rather than a member of it: `options` is the
Mountebank-compatible shape and is unchanged.

**Reading this from the standalone binary:** there is no serve-options document in process mode —
that door only exists for an embedded host going through `rift_serve_admin`. The list is still a
faithful capability signal for the running release, but a process-mode consumer sets the equivalent
lever on the command line instead: `requireAdminAuth` → `--require-admin-auth`, `allowInjection` →
`--allow-injection`, `configFile` → `--configfile`, `noParse` → `--no-parse`, `apiKey` → `--api-key`,
and so on.

---

## Logs

### GET /logs

Mountebank-compatible stub. Rift writes its logs through `tracing` (stderr), not to an in-memory
buffer, so this always returns an empty list:

```json
{ "logs": [], "_links": { "self": { "href": "/logs?startIndex=0&endIndex=100" } } }
```

**Query Parameters:** `startIndex` (default `0`) and `endIndex` (default `100`) are parsed and only
echoed in `_links.self`.

---

## Error Responses

Every error Rift serves in the Mountebank `errors` envelope — on the admin plane and on the imposter
port alike — carries three fields:

```json
{ "errors": [ { "code": "...", "type": "...", "message": "..." } ] }
```

**One door carries extra fields.** A backend outage (an unavailable flow-store or proxy store, and
any matcher failure that is not an injection error) enriches the envelope's error object:

```json
{
  "errors": [{
    "code": "503",
    "type": "backend unavailable",
    "message": "flowState: redis connection refused",
    "feature": "flowState",
    "detail": "redis connection refused"
  }]
}
```

`feature` names *which* backend failed and `detail` gives the underlying cause.

> **Removed in 0.18.0:** this door used to duplicate `error`/`feature`/`detail` as **top-level**
> keys (the pre-0.16.0 shape, deprecated in 0.16.0, #801). They are gone; read `errors[0]`.

| Field | Read it? | What it is |
|:------|:---------|:-----------|
| `type` | **yes** | The stable symbolic error type. Always a lowercase slug, on every door. This is the field to branch on. |
| `code` | legacy | The HTTP status as a string on most doors, but a slug on a few (`invalid injection`, `unauthorized`, …). Frozen for backward compatibility — it is *not* reliably parseable as either. |
| `message` | human | Free text. Never pattern-match it. |

`type` was added in 0.15.0 (issue #797). `code` is unchanged from earlier releases, so existing
clients keep working; new clients should read `type` instead.

### Error types

Six are Mountebank's own error types, so a client that already maps Mountebank errors keeps working:

| `type` | Typical status |
|:-------|:---------------|
| `bad data` | 400, 422 |
| `unauthorized` | 401 |
| `insufficient access` | 403 |
| `no such resource` | 404 |
| `resource conflict` | 409 |
| `invalid injection` | 400 |

The rest name doors Mountebank does not have:

| `type` | Typical status |
|:-------|:---------------|
| `invalid predicate injection` | 400 |
| `injection timeout` / `predicate injection timeout` | 504 |
| `backend unavailable` | 503 |
| `script error` / `script timeout` | 500 / 504 |
| `behavior error` | 500 |
| `imposter disabled` | 503 |
| `request too large` | 413 |
| `upstream failure` | 502 |
| `unavailable` | 503 |
| `timeout` | 504 |
| `internal error` | 500 |
| `client error` / `server error` | any other 4xx / 5xx |

### 400 Bad Request

Invalid request body or parameters.

```json
{
  "errors": [
    {
      "code": "400",
      "type": "bad data",
      "message": "invalid JSON"
    }
  ]
}
```

### 404 Not Found

Imposter doesn't exist.

```json
{
  "errors": [
    {
      "code": "404",
      "type": "no such resource",
      "message": "Imposter not found on port 4545"
    }
  ]
}
```

### 409 Conflict

A stub with that `id` already exists on the imposter.

```json
{
  "errors": [
    {
      "code": "409",
      "type": "resource conflict",
      "message": "A stub with id 'login' already exists"
    }
  ]
}
```

Note that a port already being in use is **not** a 409 — it is a `400` / `bad data`, because the
request describes an imposter that cannot be created:

```json
{
  "errors": [
    {
      "code": "400",
      "type": "bad data",
      "message": "Port 4545 is already in use"
    }
  ]
}
```

### 413 Payload Too Large

The request body exceeds the admin API's size limit (64 MiB). The limit bounds
how much of a single request Rift buffers into memory, since the admin plane
binds `0.0.0.0` and `--apikey` is optional.

```json
{
  "errors": [
    {
      "code": "413",
      "type": "request too large",
      "message": "Request body exceeds the 67108864-byte admin API limit"
    }
  ]
}
```

---

## Common Patterns

### Export and Reimport

```bash
# Export
curl "http://localhost:2525/imposters?replayable=true" > imposters.json

# Clear
curl -X DELETE http://localhost:2525/imposters

# Reimport
curl -X PUT http://localhost:2525/imposters \
  -H "Content-Type: application/json" \
  -d @imposters.json
```

### Verify Requests

```bash
# Create imposter with recording
curl -X POST http://localhost:2525/imposters \
  -H "Content-Type: application/json" \
  -d '{
    "port": 4545,
    "protocol": "http",
    "recordRequests": true,
    "stubs": [...]
  }'

# Run tests...

# Verify requests
curl http://localhost:2525/imposters/4545 | jq '.requests'
```
