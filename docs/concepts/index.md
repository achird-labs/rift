---
layout: default
title: Concepts
nav_order: 3
has_children: true
permalink: /concepts/
---

# Concepts

Rift is a high-performance mock server. It speaks the [Mountebank](https://www.mbtest.dev/) API for
compatibility, but it is a tool in its own right — with a stateful model (flow-state, scenarios,
correlated isolation) that goes well beyond record/replay. This section explains the mental model
once, conceptually, so the reference and feature pages make sense.

Start here if you're new to Rift; jump to the reference pages when you need the exact syntax.

---

## The two layers

Rift configuration has two layers that compose:

1. **The Mountebank layer** — imposters, stubs, predicates, responses, and behaviors. If you know
   Mountebank, this is unchanged, and your existing configs work as-is. See
   [Core Building Blocks]({{ site.baseurl }}/concepts/building-blocks/).
2. **The Rift layer** (`_rift`) — stateful and fidelity features Rift adds on top: flow-state,
   scenarios, correlated isolation ("spaces"), fault injection, scripting, and response templating.
   See [The Rift Model]({{ site.baseurl }}/concepts/rift-model/).

Everything Rift-specific lives under a `_rift` key (imposter- or response-level) or under
Rift-recognised stub fields, so a plain Mountebank config never collides with it.

---

## The request lifecycle

When a request reaches an imposter, Rift:

1. **Accepts the connection.** Usually on the imposter's own port, but a request can also arrive
   through the [front door]({{ site.baseurl }}/features/front-door/) or the
   [single-port gateway]({{ site.baseurl }}/features/gateway/), which dispatch it in-process to the
   same imposter. For `https` imposters Rift terminates TLS here — and, with `mutualAuth`, demands a
   client certificate ([TLS]({{ site.baseurl }}/features/tls/)). HTTP/1.1 or HTTP/2 is settled per
   connection, by ALPN over TLS or by the connection preface in cleartext.
2. **Records the request** in the imposter's journal when `recordRequests` is on — before
   matching, so a request that matches nothing is still visible.
3. **Matches** it against each stub's [predicates]({{ site.baseurl }}/mountebank/predicates/) and
   picks the first stub, in declaration order, whose predicates all pass. With the `X-Rift-Debug`
   header, Rift explains the match instead of serving it
   ([Debug Mode]({{ site.baseurl }}/features/debug-mode/)). If nothing matches, the imposter's
   `defaultForward` or `defaultResponse` answers.
4. **Selects a response** from that stub's `responses` (cycling through them, honoring `repeat`).
5. **Resolves the response** — a static `is` (with `${request.*}` interpolation and
   [templates]({{ site.baseurl }}/features/date-templates/)), a `proxy` to an upstream, or a
   script/`inject` — and applies any [fault injection]({{ site.baseurl }}/features/fault-injection/).
6. **Runs behaviors** — latency (`wait`), `copy`/`lookup`, and `decorate`/`shellTransform`
   transforms — before sending. (See
   [Behaviors]({{ site.baseurl }}/mountebank/behaviors/#behavior-order) for the exact order.)

State (flow-state, scenario state, response cursors) is read and written along the way, keyed by the
request's **flow id**.

Everything above belongs to the imposter. The [intercept proxy]({{ site.baseurl }}/features/intercept-proxy/)
is a separate listener in front of all of this: it terminates a `CONNECT` tunnel with a certificate
signed by its own CA, matches each decrypted request against its own rules with the same predicate
engine, and either serves an inline stub or forwards the request into an imposter, where the
lifecycle above applies. An embedder can veto or replace a live exchange through the
[`ExchangeInspector`]({{ site.baseurl }}/embedding/spi/) hook, which sees the request between steps
2 and 3 and the finished response after step 6.

---

## In this section

- [Core Building Blocks]({{ site.baseurl }}/concepts/building-blocks/) — imposters, stubs,
  predicates, responses, behaviors.
- [The Rift Model]({{ site.baseurl }}/concepts/rift-model/) — flow id, flow-state, scenarios, and
  correlated isolation (spaces), and how they fit together.
