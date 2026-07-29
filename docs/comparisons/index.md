---
layout: default
title: Comparisons
nav_order: 5.7
has_children: true
---

# Comparisons

How Rift relates to the other mock servers you might be choosing between.

- [Rift vs WireMock]({{ site.baseurl }}/comparisons/wiremock/) — the JVM incumbent, and the
  comparison most teams evaluating Rift actually care about.

For Mountebank, the relationship is different in kind: Rift implements Mountebank's admin API
and loads its `imposters.json` unchanged, so the relevant page is
[Mountebank Compatibility]({{ site.baseurl }}/mountebank/) rather than a comparison, plus the
[migration guide]({{ site.baseurl }}/getting-started/migration).

---

## How we write these

Three rules, so you can calibrate how much to trust the numbers:

1. **Every performance claim names its hardware, date, engine versions, and the offered
   concurrency.** Ratios move with concurrency; one without a connection count attached is
   not a measurement.
2. **The other engine gets tuned.** We configure the thing we are comparing against the way
   its own documentation says to, publish what we changed, and publish the untuned column
   beside it so you can see whether the tuning mattered.
3. **We publish the scenarios where the comparison is not like-for-like**, and say why. See
   [the two caveats we do not bury]({{ site.baseurl }}/performance/#two-caveats-we-will-not-bury).

The full harness lives in [`tests/benchmark`](https://github.com/achird-labs/rift/tree/master/tests/benchmark)
and runs both engines, not just ours. If you think a comparison is unfair, the fastest way to
show it is to re-run it — and we would rather publish a corrected number than a flattering one.
