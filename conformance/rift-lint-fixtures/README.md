# rift-lint diagnostic fixtures (#204)

One minimal file per diagnostic code, named after the code it must trigger. `RiftToolsCli204Spec`
lints each with `rift-lint -o json` and asserts that the code is reported at its severity. JSON
cannot carry a header comment, so this table records what each file exercises. The codes are
defined in rift's
[docs/features/linting.md](https://github.com/achird-labs/rift/blob/v0.18.1/docs/features/linting.md).

| File | Code | Severity | Trigger | rift |
|---|---|---|---|---|
| `E013.json` | E013 | error | invalid `matches` regex nested under `and` | #1221 |
| `E025.json` | E025 | error | negative `wait` | linting.md |
| `E041.json` | E041 | error | `_rift.fault.tcp.probability` 1.5 | linting.md |
| `E043.json` | E043 | error | `proxy.injectHeaders` names a header twice, in different case | #1062 |
| `E044.json` | E044 | error | `proxy.injectHeaders` names a header twice, byte-identically | #1069 |
| `E045.json` | E045 | error | `proxy.injectHeaders` value is a number | #1069 |
| `E046.yaml` | E046 | error | YAML root is a mapping, not a sequence | #1071 |
| `E047.json` | E047 | error | `port` is a string | #1088 |
| `E048.json` | E048 | error | `_behaviors` is an array | #1099, #1101 |
| `E049.json` | E049 | error | an EJS tag the engine does not evaluate | #1108 |
| `E050.json` | E050 | error | `intercept.returnCaKey: true` in a config file | linting.md |
| `E051.json` | E051 | error | a `copy` item that is not an object | linting.md |
| `W012.json` | W012 | warning | a number literal that is not exactly representable as a double | #1083 |
| `W013.json` | W013 | warning | `process.env.RIFT_LINT_T_204_UNSET` with no default, unset | #1108, #1116 |
| `W014.json` | W014 | warning | a script uses `ctx.state` with no `_rift.flowState` | #1156 |
| `W016.json` | W016 | warning | `defaultEngine: "lua"` | linting.md |
| `W018.json` | W018 | warning | one `behaviors` element sets two behaviors | linting.md |
| `I005.json` | I005 | info | a `_rift.dataset` carrier | linting.md |
