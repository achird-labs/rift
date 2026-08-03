---
layout: default
title: Java / JVM
parent: Language SDKs
nav_order: 1
permalink: /sdk/java/
---

# rift-java

The official JVM SDK. Fluent typed DSL, all three transports, and JUnit 5, Spring and
Testcontainers integrations.

## Install

```xml
<dependency>
  <groupId>io.github.achird-labs</groupId>
  <artifactId>rift-java-core</artifactId>
  <version>0.2.2</version>
  <scope>test</scope>
</dependency>
```

The [BOM](https://github.com/achird-labs/rift-java/blob/master/rift-java-bom/README.md) pins every
module at once if you use more than one.

For the embedded transport, add `rift-java-embedded` (JDK 22+ bytecode, stable Panama FFM) or
`rift-java-embedded-jdk21` on JDK 21. Connect and spawn need neither and run on JDK 17+.

## Hello world

```java
try (Rift rift = Rift.embedded()) {                  // or Rift.connect(uri) / Rift.spawn()
    Imposter users = rift.create(
        imposter("users").stub(onGet("/api/users/1").willReturn(okJson("{\"id\":1}"))));

    // point your system under test at users.uri(), then assert:
    users.verify(onGet("/api/users/1"), times(1));
}
```

`Rift.embedded()` runs the engine in-process over Panama FFM — no separate binary and no container.
`Rift.spawn()` manages a downloaded binary for you, and `Rift.connect(uri)` targets any running
admin endpoint.

## Further reading

- [rift-java documentation](https://achird-labs.github.io/rift-java/) — the full feature surface,
  JUnit 5 extension, Spring and Testcontainers integrations
- [rift-java on GitHub](https://github.com/achird-labs/rift-java)
