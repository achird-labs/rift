# rift-types

Shared wire types for the [Rift](https://github.com/achird-labs/rift) workspace.

This crate holds the serde data types that more than one Rift crate has to agree on — today the
Mountebank predicate model (`Predicate`, `PredicateOperation`, `PredicateParameters`,
`PredicateSelector`) — and the `wire` module, which carries the shared serde rules for how those
values are spelled on the wire (for example, how a single-valued header map is read).

It has no behaviour and no Rift dependencies. `rift-mock-core` and `rift-http-proxy` depend on it.

You normally do not depend on this crate directly: `rift-mock-core` re-exports what an embedder
needs. See [Embedding & SPI](https://achird-labs.github.io/rift/embedding/).

## License

Apache-2.0
