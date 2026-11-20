#!/usr/bin/env python3
"""Every function and interface in the rift:mock WIT is documented in docs/features/wasm-wit.md
(issue #1797). verify-docs-coverage.sh covers CLI flags and env vars, not the WIT, so without this a
new function could ship with no specified semantics.

A function counts as documented when the page names it as code with its parameters,
`name(` (for example `incr-by(key, by, flow)`); an interface when it appears as `name`.

Usage: scripts/verify-wit-docs.py [--self-test]
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
WIT = ROOT / "crates/rift-mock-core/wit/rift-mock.wit"
DOC = ROOT / "docs/features/wasm-wit.md"

FUNC = re.compile(r"^\s*([a-z][a-z0-9-]*)\s*:\s*func\(", re.M)
IFACE = re.compile(r"^\s*interface\s+([a-z][a-z0-9-]*)\s*\{", re.M)


def missing(wit: str, doc: str) -> list[str]:
    out = []
    for name in IFACE.findall(wit):
        if f"`{name}`" not in doc and f"`{name}:" not in doc:
            out.append(f"interface {name}")
    for name in FUNC.findall(wit):
        if f"`{name}(" not in doc:
            out.append(f"function {name}")
    return out


def self_test() -> None:
    wit = "interface state {\n  get: func(key: string);\n  put: func(v: string);\n}\n"
    assert missing(wit, "`state` `get(key)`") == ["function put"], missing(wit, "`state` `get(key)`")
    assert missing(wit, "`get(` `put(`") == ["interface state"]
    assert FUNC.findall("incr-by: func(key: string, by: s64)") == ["incr-by"]
    print("[OK] self-test")


def main() -> int:
    if "--self-test" in sys.argv[1:]:
        self_test()
        return 0
    gaps = missing(WIT.read_text(), DOC.read_text())
    funcs = len(FUNC.findall(WIT.read_text()))
    if gaps:
        for gap in gaps:
            print(f"[FAIL] {gap} is in {WIT.relative_to(ROOT)} but not in {DOC.relative_to(ROOT)}")
        return 1
    print(f"[OK] all {funcs} rift:mock functions are documented")
    return 0


if __name__ == "__main__":
    sys.exit(main())
