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
