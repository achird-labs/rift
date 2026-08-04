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

When a request hits an imposter, Rift:

1. **Matches** it against each stub's [predicates]({{ site.baseurl }}/mountebank/predicates/), in
   order, and picks the first stub whose predicates all pass.
2. **Selects a response** from that stub's `responses` (cycling through them, honoring `repeat`).
3. **Resolves the response** — a static `is`, a `proxy` to an upstream, or a script/`inject` — and
   applies any [fault injection]({{ site.baseurl }}/features/fault-injection/).
4. **Runs behaviors** — latency (`wait`), request interpolation, `copy`/`lookup`, and
   `decorate`/`shellTransform` transforms — before sending. (See
   [Behaviors]({{ site.baseurl }}/mountebank/behaviors/#behavior-order) for the exact order.)

State (flow-state, scenario state, response cursors) is read and written along the way, keyed by the
request's **flow id**.

---

## In this section

- [Core Building Blocks]({{ site.baseurl }}/concepts/building-blocks/) — imposters, stubs,
  predicates, responses, behaviors.
- [The Rift Model]({{ site.baseurl }}/concepts/rift-model/) — flow id, flow-state, scenarios, and
  correlated isolation (spaces), and how they fit together.
