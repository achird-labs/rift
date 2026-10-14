---
layout: default
title: Differences from Mountebank
parent: Mountebank Compatibility
nav_order: 6
---

# Differences from Mountebank

Rift answers the Mountebank API the way Mountebank 2.9.1 does, with the deliberate exceptions on
this page. Every one of them is pinned by the
[differential harness](https://github.com/achird-labs/rift/tree/master/conformance/differential): it
runs each test case on a real Mountebank and on Rift, and fails on any difference that its allow-list
does not cite from these docs or name as a known bug. A behaviour that differs and is *not* listed
here is a bug.

## Marker headers on served responses

Every response an imposter serves carries `x-rift-imposter: true`. Mountebank sends only the headers
the response configures. Some responses carry one more marker:

| Header | When |
|:-------|:-----|
| `x-rift-proxy: true` | The response came from a `proxy` |
| `x-rift-proxy-latency: <ms>` | A proxied response, with the upstream's time in milliseconds |
| `x-rift-proxy-error: true` | The proxy could not reach the upstream (see [Proxy](proxy.md)) |
| `x-rift-inject: true` | An `inject` response produced it |
| `x-rift-default-response: true` | No stub matched and the imposter's `defaultResponse` answered |
| `x-rift-no-match: true` | No stub matched and the imposter has no `defaultResponse` |

A proxy whose upstream is itself a Rift imposter therefore records that imposter's markers in the
saved response's `headers`, like any other upstream header.

## The admin API serves JSON only

Rift's admin API has no HTML views, so it does not negotiate on `Accept`: it sends no `Vary: Accept`
header, and a route it does not have answers a JSON error envelope where Mountebank answers an HTML
error page.

## Error envelopes

Error envelopes carry `code` (the HTTP status), `type` and `message` — see the
[API reference](../api/index.md) — where Mountebank's carry `code` (Rift's `type`) and `message`. Rift
does not add Mountebank's `source`, `name` or `stack` fields, and its messages are worded
differently.

## Request header names

Rift cannot see the spelling a client used for a header name: an imposter sees names Title-Cased, and an inject or decorate script finds a header under any spelling. Mountebank passes scripts the names exactly as the client wrote them, so
`request.headers['content-type']` is `undefined` there when the client sent `Content-Type`, and
the reverse. In Rift both spellings find the header; `Object.keys(request.headers)` and
`JSON.stringify(request.headers)` show the Title-Cased names (`Content-Type`). The same root cause
shows in two places that are not scripts: a header-name predicate with `caseSensitive: true` (or
`keyCaseSensitive: true`) compares against the Title-Cased name, and `savedRequests` record the
Title-Cased name.

## `decorate` and a response without headers

`decorate` always receives a `headers` object on the response, empty when the response has none; in Mountebank `response.headers` is `undefined` there, so a decorate that writes a header fails. Rift accepts that script, so `config.response.headers['X-D'] = 'y'` on an `is` with no headers
adds the header. More generally, in Mountebank 2.9.1 any decorate that throws answers `500` with a
JavaScript stack trace in the body (its error handler fails while logging the request). Rift serves
the undecorated response with `x-rift-decorate-error: true` instead, and `500` only under
`strictBehaviors` — see [Behaviors](behaviors.md).

## keyCaseSensitive is honoured as written

Rift applies `keyCaseSensitive` whenever it is set, on every operator; Mountebank applies it only to turn key matching off under `caseSensitive: true`. Three consequences:

- `keyCaseSensitive: true` on its own makes the names in `query`, `headers` and `form` compare
  exactly. In Mountebank it does nothing unless `caseSensitive: true` is also set.
- Under `matches`, Rift honours the setting. Mountebank replaces it with `caseSensitive`, so
  `caseSensitive: true, keyCaseSensitive: false` compares keys exactly there and relaxes them in Rift.
- With a `jsonpath` selector that has an upper-case letter, `keyCaseSensitive: true` still matches in
  Rift; in Mountebank it never matches (the selector keeps its case while the body's keys are
  lowercased).

Header names are compared against the Title-Cased names the listener stores (`X-Tenant`), because
Rift cannot see the spelling the client sent (see [Request header names](#request-header-names)).
The setting is described in [Predicates](predicates.md#keycasesensitive).

## `/imposters/:port/requests` means recorded requests

`DELETE /imposters/:port/requests` clears the imposter's recorded requests, like `DELETE /imposters/:port/savedRequests`, and `GET /imposters/:port/requests` reads them. In Mountebank the `DELETE` is a deprecated alias of `DELETE /imposters/:port/savedProxyResponses`
and leaves recorded requests alone, and there is no `GET`. To clear saved proxy responses on Rift, use
`savedProxyResponses`.

## A proxy without generators records into a store, not a stub

A proxy without `predicateGenerators` (and without `addWaitBehavior`/`addDecorateBehavior`) records into an internal store keyed per request, not into a stub, so `GET /imposters/:port` and `?replayable=true` do not show its recordings. Mountebank records an empty-predicate stub there, which answers every later request with the
first response, whatever its path or method. Rift replays per request instead (method, path, query
string and body). To record exportable stubs, give the proxy `predicateGenerators`, for example
`{"matches": {"method": true, "path": true, "query": true, "body": true}}`. See
[Proxy](proxy.md).

## Generated predicates

A proxy's `predicateGenerators` record predicates whose order, selector scope, selected-value type
and header-name lookup differ from Mountebank 2.9.1's; each difference, and why Rift's recording is
the one that replays, is listed under [Proxy: What Gets Recorded](proxy.md#what-gets-recorded).

## `defaultResponse` completes a response by the fields it omits

An imposter's `defaultResponse` is served when no stub matches, and it also supplies the `statusCode`, `headers`, `body` and `_mode` that an `is` or `inject` response omits, as in Mountebank. `headers` is taken whole, never merged key by key; a `proxy` response is never completed. Mountebank decides by falsiness and Rift by whether the field was written, which differs in two places:

`defaultResponse` completes a response by the fields it omits: an explicit `"body": ""` is served empty, and a proxied empty body is served as the upstream sent it; Mountebank replaces both with the `defaultResponse` body.

`defaultResponse` is merged before behaviors run. Mountebank merges after them, so there a `decorate` reading `config.response.statusCode` on a response that omits it sees `undefined`; in Rift the decorate sees the completed response, and a default body is expanded like a stub body.

A `proxy` inside `defaultResponse` is kept in the printed and persisted imposter and reported as ignored in `_rift.warnings`: neither engine proxies from it. Use `defaultForward` to forward unmatched requests.

## Printed imposters are normalized

Mountebank prints a stub exactly as it was posted. Rift prints what it parsed, so its output can be
spelled differently from the input while meaning the same thing. Re-posting what Rift prints gives
the same imposter.

- `statusCode` prints as a JSON number in a stub's `is`, as it already did in `defaultResponse`. A status posted as a string prints as a number (`"202"` prints as `202`).
- A field left empty or at its default is not printed. This covers a stub's empty `predicates`,
  whether written or omitted, an empty `defaultResponse.headers`, and a proxy's default `mode`,
  `predicateGenerators`, `addWaitBehavior` and `injectHeaders`. Each reads back to the same value.
  A `headers: {}` written in a stub's `is` is still printed, because it replaces the
  `defaultResponse` headers with none.
- Empty `behaviors` and a `null` response variant such as `proxy: null` are accepted and not printed back. Each means nothing, so it reads back the same. Mountebank echoes both as posted, and its `removeProxies` export then drops every response written with a `proxy` key, `proxy: null` included.
- The imposter list carries `recordRequests` for each imposter, next to Rift's `stubCount` and `enabled`. Mountebank prints `recordRequests` only in an imposter's detail.
- The `?replayable=true` export keeps the imposter's `host`, so a re-posted export binds where the original did. Mountebank 2.9.1's export leaves `host` out.

## Elsewhere in these docs

Each of these is described where the feature is:

- Served JSON bodies have their keys sorted — [Responses](responses.md).
- `copy` with a `regex` returns the first capture group — [Behaviors](behaviors.md).
- `shellTransform` output is served as the body, not read as a response object — [Behaviors](behaviors.md).
- A `lookup` after a `copy` does not expand a token the client put in the copied field — [Behaviors](behaviors.md).
- A `wait` range `{"min", "max"}` is a Rift extension — [Behaviors](behaviors.md).
- String predicate operators fold ASCII case only; `matches` folds Unicode — [Predicates](predicates.md).
- An unreachable proxy upstream answers `502` — [Proxy](proxy.md).
- `protocol` is optional and defaults to `http` — [Imposters](imposters.md).
- A proxy-recorded stub carries `recordedFrom` — [Imposters](imposters.md).
- Rift-only config keys print with `?replayable=true`, and the error envelope carries `type` — [API](../api/index.md).
