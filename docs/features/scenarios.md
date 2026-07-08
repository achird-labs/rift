---
layout: default
title: Scenarios (FSM)
parent: Features
nav_order: 20
---

# Scenarios (Stateful FSM)

Scenarios let a stub respond differently depending on where a flow is in a state machine — a
WireMock/Mountebank-style FSM. Each stub can require a state to be eligible and can transition the
state after it responds.

---

## Stub fields

| Field | Meaning |
|:------|:--------|
| `scenarioName` | Names the scenario this stub belongs to. |
| `requiredScenarioState` | The stub is only eligible when the scenario is in this state. |
| `newScenarioState` | After the stub responds, the scenario transitions to this state. |

`newScenarioState` is optional. If a matched stub omits `newScenarioState`, the scenario remains in
its current state — no transition happens. That lets a stub gate on state without ever advancing
it, which is how you model a repeatable step in the middle of a flow.

The implicit initial state is **`Started`**. State is tracked per flow id (see
[Spaces]({{ site.baseurl }}/features/spaces/)); by default that is the imposter port, so a single
imposter has one scenario timeline.

---

## Example — pay-then-fulfil

The first call to `/pay` returns `402` and moves the scenario to `paid`; subsequent calls match the
second stub and return `200`.

```json
{
  "port": 4602,
  "protocol": "http",
  "stubs": [
    {
      "scenarioName": "checkout",
      "requiredScenarioState": "Started",
      "newScenarioState": "paid",
      "predicates": [{ "equals": { "path": "/pay" } }],
      "responses": [{ "is": { "statusCode": 402, "body": "payment required" } }]
    },
    {
      "scenarioName": "checkout",
      "requiredScenarioState": "paid",
      "predicates": [{ "equals": { "path": "/pay" } }],
      "responses": [{ "is": { "statusCode": 200, "body": "fulfilled" } }]
    }
  ]
}
```

```bash
curl -i http://localhost:4602/pay   # 402 payment required  (now in state "paid")
curl -i http://localhost:4602/pay   # 200 fulfilled
```

---

## Parallel callers, independent timelines

By default all callers share one scenario timeline (the flow id defaults to the imposter port). To
let two callers advance the *same* `scenarioName` independently, scope the flow id to a request
header — e.g. `flowIdSource: "header:X-Mock-Space"` — so each caller's `X-Mock-Space` value gets its
own state:

```json
{
  "port": 4602,
  "protocol": "http",
  "_rift": {
    "flowState": { "flowIdSource": "header:X-Mock-Space" }
  },
  "stubs": [
    {
      "scenarioName": "checkout",
      "requiredScenarioState": "Started",
      "newScenarioState": "paid",
      "predicates": [{ "equals": { "path": "/pay" } }],
      "responses": [{ "is": { "statusCode": 402, "body": "payment required" } }]
    },
    {
      "scenarioName": "checkout",
      "requiredScenarioState": "paid",
      "predicates": [{ "equals": { "path": "/pay" } }],
      "responses": [{ "is": { "statusCode": 200, "body": "fulfilled" } }]
    }
  ]
}
```

Alice and bob each drive the `checkout` scenario at their own pace — one being mid-flow doesn't
gate or advance the other's state:

```bash
curl -i -H 'X-Mock-Space: alice' http://localhost:4602/pay   # 402  (alice: Started -> paid)
curl -i -H 'X-Mock-Space: bob'   http://localhost:4602/pay   # 402  (bob: Started -> paid, independent of alice)
curl -i -H 'X-Mock-Space: alice' http://localhost:4602/pay   # 200  (alice already paid)
curl http://localhost:2525/imposters/4602/scenarios?flowId=bob
# -> {"flowId":"bob","scenarios":[{"name":"checkout","state":"paid"}]}
```

See [Correlated Isolation (Spaces)]({{ site.baseurl }}/features/spaces/) for how `flowIdSource` is
resolved.

---

## Arranging and inspecting state

Only scenarios declared on stubs are tracked; there is no upfront registration.

```bash
# List scenario states (optionally scoped to a flow with ?flowId=)
curl http://localhost:2525/imposters/4602/scenarios
# -> {"flowId":"4602","scenarios":[{"name":"checkout","state":"paid"}]}

# Force a scenario into a state (body flowId optional)
curl -X PUT http://localhost:2525/imposters/4602/scenarios/checkout/state \
  -d '{"state":"paid"}'

# Reset every scenario in a flow back to "Started"
curl -X POST http://localhost:2525/imposters/4602/scenarios/reset -d '{}'
```

See the [API Reference]({{ site.baseurl }}/api/#scenarios) for the full endpoint contract.
