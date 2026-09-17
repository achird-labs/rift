---
layout: default
title: The Rift Model
parent: Concepts
nav_order: 2
---

# The Rift Model

Plain Mountebank is essentially stateless: a request matches a stub and gets a response. Rift adds a
**stateful layer** so a response can depend on what happened before — retry-then-succeed, call
counters, multi-step workflows, and per-test isolation. Three concepts, all tied together by one
idea: the **flow id**.

---

## Flow id — the correlation key

Every request resolves to a **flow id**: the identity Rift uses to correlate a request with the state
that belongs to it. How it's derived is set per imposter by `_rift.flowState.flowIdSource`:

- **`imposter_port`** (default) — all traffic to the imposter shares one flow. Simple global state.
- **`header:<Name>`** — the flow id is taken from a request header (e.g. `header:X-Flow-Id`), so each
  caller/test/session gets its own isolated state.

The same flow id drives both **flow-state** and **spaces**, which is what lets them stay consistent.

---

## Flow-state — a per-flow key/value store

[Flow-state]({{ site.baseurl }}/features/flow-state/) is a `(flow_id, key)` → value store for
stateful behavior. [Scripts]({{ site.baseurl }}/features/scripting/) read and write it freely; a
static response can also write it declaratively with `_rift.stateOps` and read it back with a
{% raw %}`{{ state.<key> }}`{% endraw %} [template]({{ site.baseurl }}/features/date-templates/), no script needed. Either
way you can count attempts, fail the first N and then succeed, or gate on a stored value:

```
attempt 1 → 503   attempt 2 → 503   attempt 3 → 200
```

Backends are `inmemory` (default) or `redis` (shared across instances); an embedder can register
its own. Values expire after `ttlSeconds`. You can also inspect and seed flow-state directly over the admin API.

## Scenarios — declarative state machines

[Scenarios]({{ site.baseurl }}/features/scenarios/) let a stub advance a named finite-state machine
**without writing a script**. A stub can require the scenario to be in a given state to match
(`requiredScenarioState`) and move it to a new state after responding (`newScenarioState`). This
models multi-step flows (e.g. `Started → InProgress → Complete`) declaratively; the admin API
inspects and resets scenario state.

## Spaces — correlated isolation

[Spaces]({{ site.baseurl }}/features/spaces/) partition an imposter's stubs and state **per flow id**,
so concurrent tests don't step on each other. Each flow gets its own view — its own stub overrides
and its own state — over the same imposter port. This is how many parallel tests share one imposter
without cross-contamination.

---

## How they fit together

- **Flow id** decides *whose* state a request touches.
- **Flow-state** is the general-purpose store for that flow, written by scripts or `stateOps`.
- **Scenarios** are a declarative state machine over that flow — no scripting needed.
- **Spaces** isolate stubs and state *between* flows.

Reach for the simplest one that fits: scenarios for a declarative multi-step flow, `stateOps` and
templates for counters and stored values, a script when the logic outgrows them, spaces when
parallel tests must not interfere. See also
[Fault Injection]({{ site.baseurl }}/features/fault-injection/) for stateful failure simulation built
on flow-state.
