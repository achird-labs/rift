---
layout: default
title: Scala 3
parent: Language SDKs
nav_order: 2
permalink: /sdk/scala/
---

# rift-scala

The official Scala 3 SDK. One typed model and DSL, with a thin module per effect system — pick the
surface that matches your stack rather than adapting to someone else's.

## Install

```scala
// ZIO 2
libraryDependencies += "io.github.achird-labs" %% "rift-scala-zio" % "0.1.3" % Test

// Cats Effect 3 (+ FS2)
libraryDependencies += "io.github.achird-labs" %% "rift-scala-cats" % "0.1.3" % Test

// No effect system — Either-based, Using-friendly
libraryDependencies += "io.github.achird-labs" %% "rift-scala-pure" % "0.1.3" % Test
```

Requires JDK 21+. The embedded transport comes through the rift-java bridge, so it needs JDK 22+
(or the `rift-java-embedded-jdk21` artifact on JDK 21); connect and spawn run on JDK 21+.

Testkit and codec side-cars are separate modules — `rift-scala-zio-testkit`,
`rift-scala-cats-testkit`, `rift-scala-fs2`, `rift-scala-zio-json`, `rift-scala-circe` — plus
`rift-scala-zio-bdd`, a [zio-bdd](https://github.com/EtaCassiopeia/zio-bdd) `MockControl` adapter.

## Hello world

```scala
import zio.*
import zio.test.*

import rift.dsl.*
import rift.zio.Rift

object PaymentsSpec extends ZIOSpecDefault:

  def spec = suite("payments")(
    test("records the lookup"):
      for
        users <- Rift.create(
          imposter("users").record.stub(
            get("/api/users/1").reply(ok.json("""{"id":1}"""))
          )
        )
        _ <- callSut(users.uri) // point your SUT at users.uri
        _ <- users.verify(get("/api/users/1"), 1)
      yield assertCompletes
  ).provideShared(Rift.embedded)
```

`Rift.embedded` needs no Docker and no separate binary. Swap it for `Rift.connect(uri)`,
`Rift.spawn()` or `Rift.container()` without touching the test body.

## Further reading

- [rift-scala documentation](https://achird-labs.github.io/rift-scala/) — module map, ZIO / Cats
  Effect / FS2 lifecycles, and the test-framework integrations
- [rift-scala on GitHub](https://github.com/achird-labs/rift-scala)
