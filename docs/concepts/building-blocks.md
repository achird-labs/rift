# Core Building Blocks

These are the Mountebank-compatible primitives every Rift config is built from. This page explains
what each one *is*; the reference pages under
[Mountebank Compatibility](../mountebank/index.md) give the exact syntax and every option.

---

## Imposter

An **imposter** is a mock server bound to a port. It declares a `protocol` (`http` or `https`), an
optional `name`, and a list of `stubs`. Create one by `POST`ing it to the admin API (default port
`2525`) or loading it from a config file at startup.

```json
{ "port": 4545, "protocol": "http", "stubs": [ /* … */ ] }
```

One Rift process hosts many imposters, each on its own port. An `https` imposter can also require
and validate a client certificate (`mutualAuth`, `rejectUnauthorized`, `ca`) — see
[TLS](../features/tls.md#mutual-tls-mtls). → [Imposters](../mountebank/imposters.md)

## Stub

A **stub** is a single match-and-respond rule inside an imposter: a set of **predicates** and a list
of **responses**. On each request, Rift evaluates stubs top-to-bottom and uses the **first** stub
whose predicates all match. A stub with no predicates matches everything (a good catch-all/default,
placed last). When no stub matches, the imposter answers with its `defaultResponse` — an empty
`200` unless you set one — or forwards the request to its `defaultForward` URL.

## Predicate

A **predicate** decides whether a stub applies to a request. Predicates match on request fields
(`method`, `path`, `query`, `headers`, `body`) with operators like `equals`, `deepEquals`,
`contains`, `startsWith`, `endsWith`, `matches` (regex), and `exists`, plus `jsonpath`/`xpath`
selectors and the logical combinators `and`, `or`, `not`. Multiple predicates in one stub combine
with implicit AND. → [Predicates](../mountebank/predicates.md)

## Response

A **response** is what a matched stub returns. The three kinds:

- **`is`** — a static response (`statusCode`, `headers`, `body`). Supports
  [request interpolation](../mountebank/responses.md#request-interpolation) and
  Rift's [response templates](../features/date-templates.md).
- **`proxy`** — forward to a real upstream and optionally record the reply for replay.
- **`inject`** / `_rift.script` — compute the response dynamically with a script.

A stub can list several responses; Rift cycles through them per request (round-robin), honoring each
response's `repeat` count. → [Responses](../mountebank/responses.md)

## Behavior

A **behavior** post-processes a response before it's sent. Rift supports `wait` (latency), `copy` and
`lookup` (pull request/CSV data into the response), `decorate` and `shellTransform` (transform the
body), and `repeat` (response cycling). → [Behaviors](../mountebank/behaviors.md)

---

Once these click, the [Rift Model](rift-model.md) adds state on top: making
responses depend on what happened on *previous* requests.
