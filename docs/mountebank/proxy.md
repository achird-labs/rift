# Proxy Mode

Proxy mode forwards requests to real servers and optionally records responses for playback. This is useful for creating mocks from real API behavior.

---

## Basic Proxy

Forward every request to a backend server:

```json
{
  "port": 4545,
  "protocol": "http",
  "stubs": [{
    "responses": [{
      "proxy": {
        "to": "https://api.example.com",
        "mode": "proxyTransparent"
      }
    }]
  }]
}
```

Without `"mode": "proxyTransparent"` the proxy is `proxyOnce`: it forwards the first request for
each method, path, query and body, then replays that response (see [Proxy Modes](#proxy-modes)).

---

## Proxy Modes

What a proxy records depends on whether it has `predicateGenerators` (or `addWaitBehavior` /
`addDecorateBehavior`):

- **With generators**, the proxy records each response as a new stub in the imposter, which then
  answers later matching requests. Under `proxyOnce` a request that matches no recorded stub is
  forwarded and recorded as another stub, as in Mountebank — a second body under a `body`
  generator, a second `X-Tenant` under a `headers` generator. Concurrent first requests for the
  same generated predicates record once. The recorded stub *is* the recording: delete it, or
  replace the stubs without it, and the next matching request is forwarded and recorded again.
- **Without generators**, the imposter's `stubs` never change. `"mode": "proxyOnce"` still replays:
  it keeps the first response for each request in an internal store, so the upstream sees each
  request once. A request is identified by its method, path, query string and request body;
  headers are not part of the key, so use `predicateGenerators` (for example
  `{"matches": {"headers": {"X-Tenant": true}}}`) to key on one. `DELETE /imposters/{port}/savedProxyResponses` clears that store, and the recorded responses described below. Its recordings are
  not stubs, so they do not appear in `GET /imposters/{port}` or an export; use
  `predicateGenerators` to record exportable stubs (see
  [Differences from Mountebank](differences.md)). `proxyAlways` and `proxyTransparent` forward every
  request.
- **`proxyTransparent`** never records, with or without generators.

If you leave `mode` out, it is `proxyOnce`, as in Mountebank: with generators it records stubs,
and without them it replays from the internal store. Set `"mode": "proxyTransparent"` to forward
every request. The value is read case-insensitively (`proxyalways` works; `rift-lint` W007 suggests
the canonical spelling). An unknown value such as `"proxyEverything"` is refused — `400` from
`POST /imposters` and the stub routes, a startup error from `--configfile` or `--datadir` — and `rift-lint` reports it as E052. This diverges from Mountebank, which treats an
unknown mode as `proxyOnce`.

### proxyAlways

Always forward requests; record a new stub for each unique request:

```json
{
  "proxy": {
    "to": "https://api.example.com",
    "mode": "proxyAlways"
  }
}
```

Use when: Building a comprehensive mock from varied requests.

### proxyOnce

Forward the first request, then replay the recorded response:

```json
{
  "proxy": {
    "to": "https://api.example.com",
    "mode": "proxyOnce"
  }
}
```

Use when: Recording a fixed set of responses for offline testing.

### Where recordings go

As in Mountebank, a recorded stub is placed relative to the proxy stub that answered the request:

- **`proxyOnce`**: in front of that proxy stub, so the recording answers the next matching request.
- **`proxyAlways`**: its response is added to the first stub after that proxy stub with the same
  predicates. With none, the recording goes at the **end** of the stub list, behind any stub
  written after the proxy. It answers only once the proxy stops matching, for example after
  `mb replay` or when the proxy stub is removed.

    A response added to an existing stub joins the **end of that stub's current cycle**, as in
    Mountebank's rotating response queue: if the stub has served `A` from `[A, B, C]`, the new
    response `U` is served after `B`, `C` and `A`, not straight after `C`. The stub's `responses` list
    still shows append order (`[A, B, C, U]`). Added in the middle of a `repeat` run, the response
    splits that run for good: `[A×2, B]` that has served one `A` cycles `A B A U` from then on. Any
    later edit of the stub returns it to list order, from the response that was due next.

Each recorded response carries `_proxyResponseTime` in its `is`, the upstream round trip in
milliseconds, as in Mountebank. It is the mark `DELETE /imposters/{port}/savedProxyResponses` reads:
every response carrying a numeric one is removed, a stub left empty goes, and every remaining stub
starts its cycle over. `?replayable=true` and `rift save` leave it out, so a replayed export holds
no saved proxy responses.

A `proxyAlways` recording made without `predicateGenerators` (with `addWaitBehavior` or
`addDecorateBehavior`) has no predicates. It therefore merges into the first stub after the proxy
that has none, including a catch-all stub you wrote, as in Mountebank.

### proxyTransparent

Forward without recording (pure reverse proxy):

```json
{
  "proxy": {
    "to": "https://api.example.com",
    "mode": "proxyTransparent"
  }
}
```

Use when: Acting as a transparent proxy without mocking.

---

## Predicate Generators

Control how requests are matched when generating stubs:

```json
{
  "proxy": {
    "to": "https://api.example.com",
    "predicateGenerators": [{
      "matches": {
        "method": true,
        "path": true,
        "query": true
      }
    }]
  }
}
```

### Available Fields

| Field | Description |
|:------|:------------|
| `method` | Match HTTP method |
| `path` | Match request path |
| `query` | Match query parameters |
| `headers` | Match request headers |
| `body` | Match request body |

### Generator Keys

| Key | Description |
|:----|:------------|
| `matches` | Which request fields the generated predicate captures (table above). An object whose keys are request fields: `method`, `path`, `query`, `headers`, `body`, `requestFrom`, `ip`, `form`. `requestFrom` is the client's `address:port` and `ip` its address, as the matcher reads them; a recorded `requestFrom` includes the client's port, so it matches that connection only, as in Mountebank. An imposter with no `host` listens dual-stack in both engines, so both report an IPv4 client as `::ffff:127.0.0.1` and record that; with `"host": "0.0.0.0"` the client is `127.0.0.1`. `form` is the urlencoded body, recorded like `query`; a request without a non-empty urlencoded body records no `form` predicate (Mountebank records `deepEquals: {}`, which also constrains nothing). An IPv6 client's `requestFrom` is written `[::1]:50000` in Rift and `::1:50000` in Mountebank, and under `"predicateOperator": "matches"` the brackets read as a character class, so such a recording does not match its own client |
| `predicateOperator` | The operator of every generated predicate: `equals`, `deepEquals`, `contains`, `startsWith`, `endsWith`, `matches` or `exists`; omitted, see [What Gets Recorded](#what-gets-recorded). Any other value is refused (400, or a startup error; `rift-lint` E053). Mountebank admits it and then answers `500` to every request after the first recording |
| `caseSensitive` | A boolean, copied onto each generated predicate when set (see [Case Sensitivity](#case-sensitivity)) |
| `except` | A string, copied onto each generated predicate; the recorded values stay whole and the matcher strips the regex from both sides (see [`except`](predicates.md#except)) |
| `jsonpath` | `{"selector": "$.id"}`: capture only what the selector selects from the body, and scope the generated predicate with the same selector |
| `xpath` | `{"selector": "//a:id", "ns": {"a": "urn:a"}}`: as `jsonpath`, for an XML body |
| `ignore` | Keys to leave out of a captured object field: `{"query": "ts"}`, `{"query": ["ts", "nonce"]}`, `{"headers": "X-Request-Id"}`. An object form (`{"query": {"a": …}}`) has no effect, since captured values are strings, as in Mountebank for scalar values |
| `inject` | A JavaScript function that returns the predicates (see [Generation Failures](#generation-failures)) |

With `jsonpath` or `xpath`, the captured `body` is the selected value: one match is a string, several
are an array (the recorded stub then matches a request that selects all of them), and nothing
selected is `""`. A selector that does not compile is not carried and the whole body is captured, as
without one. `ignore` applies to the object-valued fields (`query`, `headers`, `form`); it has no effect on
a string body. A key not in this table is accepted,
ignored when recording, and reported as [`config_key_ignored`](../features/stub-analysis.md#config_key_ignored)
and by `rift-lint` `W020`.

An entry the recorder cannot read is refused (400, or a startup error; `rift-lint` E055): one that
is not an object, has neither `matches` nor `inject`, has a non-string `inject`, a `matches` that is
not an object or names a key that is not a request field, a non-boolean `caseSensitive` or a
non-string `except`. Rift used to skip such an entry and record the rest, which was wider than the
generators asked for, or a stub matching every request when no entry could be read. Mountebank
refuses the first three when the imposter is created. It admits the rest: a `matches` that is not
an object, or names a key that is not a request field, records a stub that matches every request,
and a non-boolean `caseSensitive` or non-string `except` is copied onto the recorded predicates.
`caseSensitive` and `except` are not checked on an `inject` generator, which does not read them.

```json
{
  "predicateGenerators": [{
    "matches": { "path": true, "body": true, "query": true },
    "jsonpath": { "selector": "$.orderId" },
    "ignore": { "query": "ts" }
  }]
}
```

A replay then matches any request to the same path whose body has the same `orderId` and whose
query matches apart from `ts`.

### What Gets Recorded

The recorded response is an `is` with the upstream's status, headers and body, and `_mode: "text"`
(or `"binary"` for a body that is not UTF-8), as Mountebank records it. Header names are recorded
Title-Cased (`Content-Type`): Rift's HTTP client sees every upstream name lower-cased, so the
upstream's own spelling is not available, and Title-Case is what Mountebank records from a typical
HTTP/1 upstream. A name a proxy behavior wrote with an upper-case letter is kept as written. Names
are case-insensitive, and Rift serves every name lower-case on the wire either way.

A generator records **one predicate per field** in `matches`, as Mountebank does, in the fixed order
`method`, `path`, `query`, `headers`, `body`, `requestFrom`, `ip`, `form` (Mountebank's order differs; see below), each carrying the generator's `caseSensitive` and
`except` when they are set (and the body's carrying its `jsonpath` or `xpath`). The values are the
request's, unmodified. A request `GET /users/123?page=2` through

```json
{
  "predicateGenerators": [{
    "matches": { "method": true, "path": true, "query": true, "headers": { "X-Tenant": true } },
    "except": "\\d+"
  }]
}
```

records

```json
[
  { "deepEquals": { "method": "GET" }, "except": "\\d+" },
  { "deepEquals": { "path": "/users/123" }, "except": "\\d+" },
  { "deepEquals": { "query": { "page": "2" } }, "except": "\\d+" },
  { "equals": { "headers": { "X-Tenant": "acme" } }, "except": "\\d+" }
]
```

| Matcher | Recorded predicate |
|:--------|:-------------------|
| `field: true` | `{"deepEquals": {field: value}}` — the whole field, exactly. A query with an extra parameter is a different request |
| `query` / `headers: {"name": true, …}` | `{"equals": {field: {name: value, …}}}` over the listed names; a listed name the request lacks is left out |
| `"predicateOperator": "exists"` | `{"exists": {field: true}}`, or `{"exists": {field: {name: true, …}}}` for `query` and `headers` |
| any other `predicateOperator` | `{operator: {field: value}}` for each field |

A field the request lacks is recorded empty: no query is `{}`, no body is `""`.

Deviations from Mountebank:

- **Order.** Mountebank emits the predicates in the key order of `matches`; Rift's order is fixed, so two recordings of one request always build equal lists.
- **Selector scope.** Mountebank copies `jsonpath`/`xpath` onto every generated predicate, where it selects from the path or method and records `""`, so its recording never matches again and it forwards every request; Rift scopes the selector to the body predicate only.
- **Selected value.** Mountebank records the selected JSON value (`1`); Rift records its string form (`"1"`), which Mountebank also matches.
- **Listed header names.** Mountebank finds a listed header name only in the case the client sent it, and otherwise records `{}`, which matches any request; Rift finds it in any case.
- Under an explicit `predicateOperator` other than `exists`, Mountebank stores the **whole request**
  under the operator, once per field in `matches`. Rift stores that field's value only.
- A whole `query` or `headers` field the generator's `ignore` filters is recorded with `equals` over
  the kept names, so the ignored name may take any value. Mountebank records `deepEquals` without
  the ignored name and does not persist `ignore`, so its recording never matches a request that
  carries the name.

Recordings made by earlier releases hold one `equals` predicate per generator, with `except` already
applied and `caseSensitive` written; they still load and match as they did.

### Selective Matching

Match only specific aspects:

```json
{
  "predicateGenerators": [{
    "matches": {
      "method": true,
      "path": true
    }
  }]
}
```

This generates stubs that match method and path, ignoring query and body.

### Case Sensitivity

```json
{
  "predicateGenerators": [{
    "matches": { "path": true },
    "caseSensitive": true
  }]
}
```

A generated predicate carries `caseSensitive` only when the generator sets it. When it is omitted,
the predicate default applies and the recording ignores case, as in Mountebank. Set
`"caseSensitive": true` to record a predicate that compares case.

### Generation Failures

A predicate generator can also be an [`inject`](../features/scripting.md) function that builds
predicates in JavaScript. If that generation **fails** — the script throws, returns something other
than a predicate array, the script pool is unavailable, or it exceeds the script timeout — Rift does
**not** record a stub. Recording a stub with the empty/partial predicate list would produce a
match-all stub that shadows every future request, so the failure is surfaced instead of hidden:

- **no stub is recorded** for that request (the proxied response is still returned to the client), and
- the proxied response carries an `x-rift-generator-error` header whose value names the failure —
  `script-error`, `invalid-output`, `invalid-predicate`, `pool-failure`, `timeout`, or `task-panic`.

`invalid-predicate` covers any generated predicate the engine would not admit, from any generator:
an `inject` that returns a misspelled operator (`{"equal": …}`), or a `"predicateOperator":
"matches"` over a value that is not a regular expression (a path such as `/a(b`). Rift used to drop
such a predicate and record the rest; when it dropped all of them, the recording matched every
later request. It also covers an unreadable generator entry in a config that reached the engine
without the create-time check (a replayed rift-cluster config, for one).

A generator that legitimately returns an empty array (`[]`) is **not** a failure — it records a
match-all stub as before. Only genuine generation failures skip recording and set the header.

---

## Recording Workflow

### Step 1: Create Recording Proxy

```bash
curl -X POST http://localhost:2525/imposters \
  -H "Content-Type: application/json" \
  -d '{
    "port": 4545,
    "protocol": "http",
    "stubs": [{
      "responses": [{
        "proxy": {
          "to": "https://api.example.com",
          "mode": "proxyOnce",
          "predicateGenerators": [{
            "matches": { "method": true, "path": true, "query": true }
          }]
        }
      }]
    }]
  }'
```

### Step 2: Run Your Tests

```bash
# Run application tests against the proxy
npm test
# or
pytest
```

### Step 3: Export Recorded Stubs

```bash
curl http://localhost:2525/imposters/4545?replayable=true > recorded.json
```

### Step 4: Use Recorded Mocks

```bash
# Delete proxy imposter
curl -X DELETE http://localhost:2525/imposters/4545

# Load recorded mocks
curl -X POST http://localhost:2525/imposters \
  -H "Content-Type: application/json" \
  -d @recorded.json
```

### Flat Response Form

Rift accepts a stub response in **flat form** — `statusCode`, `headers`, and `body` at the top level
of the response object, with no `is` wrapper — and serves it identically to the wrapped form. This
matters when replaying recorded or externally-generated mocks that emit flat responses:

```json
// Flat form (no "is" wrapper) — accepted and served as a 200 with the body
{ "statusCode": 200, "body": "recorded" }

// Equivalent to the canonical wrapped form
{ "is": { "statusCode": 200, "body": "recorded" } }
```

`statusCode` defaults to `200` when omitted. If both a top-level field and an explicit `is` are
present, `is` takes precedence.

---

## Modifying Proxied Responses

### Behaviors on the proxy response

A `_behaviors` (or `behaviors`) block on a proxy response runs on the upstream's response before
anything is recorded, as in Mountebank. The client, the proxy recording and the stub
`predicateGenerators` generates all get the transformed response:

```json
{
  "proxy": { "to": "https://api.example.com", "mode": "proxyOnce" },
  "_behaviors": {
    "decorate": "function(request, response) { response.headers['X-Proxied-By'] = 'Rift'; }"
  }
}
```

- The generated stub holds the transformed body, not the behaviors, so a replay does not run them
  again. It carries only what `addWaitBehavior` and `addDecorateBehavior` add.
- A `proxyOnce` replay served from the recording is likewise not transformed a second time.
- A `wait` delays the live response, after the upstream's latency is measured, so it is not
  counted in what `addWaitBehavior` records.
- If a behavior fails, nothing is recorded and the next matching request goes upstream again. The
  failed response is served as usual: lenient by default (with the `x-rift-<behavior>-error`
  header), or a `500` under `strictBehaviors` (a `504` with `x-rift-script-timeout` when a
  `decorate` runs out of time). Recording it would replay a response the
  configuration did not ask for.
- A body that is not UTF-8 reaches the behaviors base64-encoded, as a `_mode: "binary"` body
  does, and is decoded before it is served.
- A request body that is not UTF-8 reaches the upstream as the bytes the client sent. Only the
  request journal, the predicates and the behaviors see its base64 form.
- The upstream's `content-length` is dropped when behaviors run, since they can change the body's
  length; the served length is computed from the body.

### addDecorateBehavior

Add a `decorate` behavior to the stub the proxy generates. It runs when that stub replays, not on
the live proxied response, as in Mountebank:

```json
{
  "proxy": {
    "to": "https://api.example.com",
    "addDecorateBehavior": "function(request, response) { \
      response.headers['X-Proxied-By'] = 'Rift'; \
      return response; \
    }"
  }
}
```

### addWaitBehavior

Record the upstream's observed latency on the generated stub, so replay reproduces it as a `wait`
behavior. It is a boolean (default `false`), as in Mountebank, and does not delay the proxied
response itself:

```json
{
  "proxy": {
    "to": "https://api.example.com",
    "addWaitBehavior": true
  }
}
```

Like `predicateGenerators`, enabling it causes a stub to be generated from the proxied response. The
live proxied response also carries an `x-rift-proxy-latency: <ms>` header when it is set.

---

## HTTPS Proxy

### Proxy to HTTPS Backend

```json
{
  "proxy": {
    "to": "https://secure-api.example.com"
  }
}
```

The origin's certificate is verified against the operating system trust store, and the connection
offers `http/1.1` only. A TLS failure on this hop (an untrusted issuer, a name mismatch) answers the
client `502` with an `x-rift-proxy-error: true` header and a JSON error envelope (`type`
`upstream failure`) whose `message` starts with `Proxy error: …` and names the upstream. The TLS
cause itself, e.g. `invalid peer certificate: UnknownIssuer`, goes to the server log only.

### Trusting a Private CA

Rift verifies outbound TLS against the operating system trust store. An origin issued by an
internal CA — a corporate API gateway — needs that CA supplied:

```bash
rift --upstream-ca-file /etc/rift/corp-ca.pem
# or
RIFT_UPSTREAM_CA_FILE=/etc/rift/corp-ca.pem rift
```

The anchor is **appended** to the OS store, so public origins keep working. The file may hold
several certificates, and a missing or unusable one stops startup. This applies to every outbound
call Rift makes: `proxy` stub upstreams, `--configfile https://…`, and (on the standalone binary) the
intercept listener's WebSocket passthrough. Embedders pass `upstreamCaFile` / `upstreamCaPem` to
`rift_serve_admin` instead. For a worked example, see
[TLS/HTTPS → Trusting a Private CA](../features/tls.md#trusting-a-private-ca).

> `SSL_CERT_FILE` / `SSL_CERT_DIR` are also honoured, but they **replace** the trust store rather
> than adding to it — pointing `SSL_CERT_FILE` at a lone private CA silently drops every public
> root. Use `--upstream-ca-file` unless you are supplying a complete bundle.

### Skipping Verification (development only)

```bash
rift --upstream-tls-skip-verify      # or RIFT_UPSTREAM_TLS_SKIP_VERIFY=true
```

Accepts any certificate and logs a warning. Prefer `--upstream-ca-file`: a recording proxy with
verification disabled will faithfully record MITM'd traffic.

> `key`, `cert`, `passphrase` and `ciphers` on a `proxy` response are accepted for Mountebank
> compatibility and **are not honoured** — they are dropped on load and do not appear when the
> imposter is read back. Rift cannot present a client certificate to an upstream, and its outbound
> trust is process-wide, configured by the two flags above.

---

## Header Manipulation

### Inject Headers

Add headers to proxied requests:

```json
{
  "proxy": {
    "to": "https://api.example.com",
    "injectHeaders": {
      "X-Forwarded-By": "Rift",
      "Authorization": "Bearer token123"
    }
  }
}
```

`injectHeaders` holds one value per name. Naming a header twice (`X-Trace` and `x-trace`) is
**rejected** — `400` from `POST /imposters`, a startup error from `--configfile` — rather than
sending two header lines, and `rift-lint` reports it before deploy (`E043`/`E044`). This diverges
from Mountebank, which keeps the last entry; see
[Single-valued header objects](imposters.md#single-valued-header-objects-reject-a-repeated-name).

---

## Path Rewriting

Modify the request path before forwarding to the backend:

```json
{
  "proxy": {
    "to": "https://api.example.com",
    "pathRewrite": {
      "from": "/api/v2",
      "to": "/api/v1"
    }
  }
}
```

### Use Cases

**Version Migration:**

Route v2 API calls to v1 backend during migration:

```json
{
  "port": 4545,
  "protocol": "http",
  "stubs": [{
    "predicates": [{ "startsWith": { "path": "/api/v2" } }],
    "responses": [{
      "proxy": {
        "to": "https://legacy-api.example.com",
        "mode": "proxyTransparent",
        "pathRewrite": {
          "from": "/api/v2",
          "to": "/api/v1"
        }
      }
    }]
  }]
}
```

**Strip Prefix:**

Remove a prefix from paths:

```json
{
  "proxy": {
    "to": "https://backend.internal",
    "mode": "proxyTransparent",
    "pathRewrite": {
      "from": "/gateway/service",
      "to": ""
    }
  }
}
```

Request to `/gateway/service/users` → forwards to `/users`

---

## Combining Proxy with Stubs

Mix static stubs with proxy fallback:

```json
{
  "port": 4545,
  "protocol": "http",
  "stubs": [
    {
      "predicates": [{ "equals": { "path": "/mocked" } }],
      "responses": [{
        "is": { "statusCode": 200, "body": "Mocked response" }
      }]
    },
    {
      "responses": [{
        "proxy": {
          "to": "https://api.example.com",
          "mode": "proxyTransparent"
        }
      }]
    }
  ]
}
```

Requests to `/mocked` return the static response; all others proxy to the real API.

---

## Example: API Gateway Pattern

Create a proxy that records and allows overriding specific endpoints:

```json
{
  "port": 4545,
  "protocol": "http",
  "stubs": [
    {
      "predicates": [{ "equals": { "path": "/health" } }],
      "responses": [{ "is": { "statusCode": 200, "body": "OK" } }]
    },
    {
      "predicates": [{ "equals": { "path": "/api/feature-flag" } }],
      "responses": [{ "is": { "body": { "enabled": true } } }]
    },
    {
      "responses": [{
        "proxy": {
          "to": "https://api.example.com",
          "mode": "proxyOnce",
          "predicateGenerators": [{
            "matches": { "method": true, "path": true }
          }]
        }
      }]
    }
  ]
}
```

---

## Best Practices

1. **Start with proxyOnce** - Record responses for consistent tests
2. **Use predicate generators wisely** - Too specific = too many stubs
3. **Export regularly** - Save recorded mocks to version control
4. **Clean up sensitive data** - Review recorded responses for secrets
5. **Use proxyTransparent for debugging** - See actual API responses
