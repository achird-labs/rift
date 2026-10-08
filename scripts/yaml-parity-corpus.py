#!/usr/bin/env python3
"""Collect the YAML parity corpus (issue #1315) into one JSON list of {name, source, yaml}.

Feed the output to the generator in crates/rift-lint/tests/yaml_parity.rs:

    python3 scripts/yaml-parity-corpus.py /tmp/cases.json
    YAML_PARITY_CASES=/tmp/cases.json  <run the rift-lint test target `yaml_parity`>

The generator writes one golden per case under crates/rift-lint/tests/fixtures/yaml-parity/. Run it
only when the YAML backend is the one whose behaviour you want to pin; the goldens are the
regression corpus for any later backend change and must never be regenerated to make a swap pass.
"""
import glob, json, os, re, sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def slug(s):
    return re.sub(r"[^a-z0-9]+", "-", s.lower()).strip("-")


RAW = re.compile(r'r(#*)"')
CHAR = re.compile(r"'(\\.|[^\\'])'")
SIMPLE = {"n": "\n", "t": "\t", "r": "\r", "0": "\0", '"': '"', "'": "'", "\\": "\\"}


def rust_literals(src):
    """Yield the value of each plain or raw string literal in Rust source (lenient)."""
    i, n = 0, len(src)
    while i < n:
        c = src[i]
        if c == "/" and src.startswith("//", i):
            j = src.find("\n", i)
            i = n if j < 0 else j
            continue
        m = None
        if c == "r" and (i == 0 or not (src[i - 1].isalnum() or src[i - 1] == "_")):
            m = RAW.match(src, i)
        if m:
            end = '"' + m.group(1)
            j = src.find(end, m.end())
            if j < 0:
                return
            yield src[m.end():j]
            i = j + len(end)
            continue
        if c == '"':
            j, out = i + 1, []
            while j < n and src[j] != '"':
                if src[j] == "\\":
                    e = src[j + 1]
                    if e == "\n":
                        j += 2
                        while j < n and src[j] in " \t\n":
                            j += 1
                        continue
                    if e in SIMPLE:
                        out.append(SIMPLE[e])
                        j += 2
                        continue
                    if e == "u":
                        k = src.index("}", j)
                        out.append(chr(int(src[j + 3:k], 16)))
                        j = k + 1
                        continue
                    if e == "x":
                        out.append(chr(int(src[j + 2:j + 4], 16)))
                        j += 4
                        continue
                    out.append(e)
                    j += 2
                    continue
                out.append(src[j])
                j += 1
            yield "".join(out)
            i = j + 1
            continue
        if c == "'":
            m2 = CHAR.match(src, i)
            i = m2.end() if m2 else i + 1
            continue
        i += 1


def looks_yaml(s):
    if "\n" not in s or s.lstrip().startswith(("{", "[", "fn ", "function")):
        return False
    return bool(re.search(r"^\s*(- )?[A-Za-z_\"'][^\n:]*:( |$)", s, re.M)) or s.lstrip().startswith("- ")


HAND = {
    "float-whole": "- port: 3000.0\n  protocol: http\n",
    "float-fraction": "- port: 3000.5\n",
    "floats-misc": "a: 1e3\nb: .5\nc: -0.0\nd: .inf\ne: -.inf\nf: .nan\ng: 1_000\nh: 0x1F\ni: 0o17\nj: 017\nk: 1.0e-2\n",
    "yaml11-scalars": "a: yes\nb: no\nc: on\nd: off\ne: y\nf: n\ng: ~\nh: null\ni: true\nj: True\nk: 0755\nl: '0755'\nm: 1:30\nn: 2001-01-01\n",
    "on-key": "on: push\nyes: 1\n",
    "block-literal": "a: |\n  line1\n  line2\n\nb: |-\n  x\n\nc: |+\n  y\n\n",
    "block-folded": "a: >\n  one\n  two\n\n  three\nb: >-\n  f\nc: >+\n  g\n\n",
    "anchor-alias": "base: &b\n  statusCode: 200\n  headers: {A: b}\nuse: *b\nlist:\n  - *b\n  - &c 5\n  - *c\n",
    "merge-key": "base: &b\n  x: 1\n  y: 2\nderived:\n  <<: *b\n  y: 3\n",
    "duplicate-key": "a: 1\na: 2\n",
    "duplicate-key-nested": "- port: 1\n  headers:\n    X: a\n    X: b\n",
    "multi-document": "- a: 1\n---\n- b: 2\n",
    "multi-document-trailing-marker": "a: 1\n---\n",
    "document-start-marker": "---\na: 1\n",
    "unclosed-flow": "port: [unclosed",
    "bad-indent": "a:\n  b: 1\n c: 2\n",
    "tab-indent": "a:\n\tb: 1\n",
    "empty": "",
    "comment-only": "# nothing\n",
    "scalar-root": "just text\n",
    "quoted-strings": "a: \"x\\ty\\u00e9\\n\"\nb: 'it''s'\nc: \"\"\nd: ''\n",
    "unicode": "k\u00e9y: \"v\u00e4lue \U0001F600\"\n",
    "tagged": "a: !!str 123\nb: !!int '7'\nc: !!float 1\nd: !custom foo\n",
    "big-ints": "a: 18446744073709551615\nb: 18446744073709551616\nc: -9223372036854775808\nd: 123456789012345678901234567890\n",
    "complex-key": "? [a, b]\n: v\n",
    "null-key-and-empty-value": "a:\nb: ~\nc: null\n",
    "flow-mapping": "{a: 1, b: [1, 2, {c: d}]}",
    "json-in-yaml": "{\"port\": 3000, \"protocol\": \"http\"}",
    "crlf": "a: 1\r\nb:\r\n  - x\r\n",
    "bom": "\ufeffa: 1\n",
}


def main(out):
    cases = []

    def add(source, i, text):
        cases.append({"name": f"{slug(source)}-{i:02d}", "source": source, "yaml": text})

    for f in sorted(glob.glob(os.path.join(ROOT, "docs/**/*.md"), recursive=True)):
        rel = os.path.relpath(f, ROOT)
        text = open(f, encoding="utf-8").read()
        for i, m in enumerate(re.finditer(r"^```ya?ml[ \t]*\n(.*?)^```", text, re.M | re.S)):
            add(rel, i, m.group(1))

    for rel in [
        "crates/rift-lint/tests/validator_tests.rs",
        "crates/rift-lint/tests/cli_output.rs",
        "crates/rift-http-proxy/tests/issue_356_yaml_file_scripts.rs",
        "crates/rift-mock-core/src/config/mod.rs",
        "crates/rift-mock-core/src/behaviors/types.rs",
        "crates/rift-mock-core/src/behaviors/wait.rs",
        "crates/rift-http-proxy/src/config_loader.rs",
    ]:
        src = open(os.path.join(ROOT, rel), encoding="utf-8").read()
        k = 0
        for lit in rust_literals(src):
            if looks_yaml(lit):
                add(rel, k, lit)
                k += 1

    # tests/compatibility/docker-compose.yml was a source too until #1341 deleted it; its golden
    # (tests-compatibility-docker-compose-yml-00.json) stays as frozen corpus, so keep it when
    # regenerating rather than deleting it with the other old goldens.
    compose = sorted(
        os.path.relpath(p, ROOT) for p in glob.glob(os.path.join(ROOT, "docs/demo/docker-compose*.yml"))
    )
    for rel in compose:
        add(rel, 0, open(os.path.join(ROOT, rel), encoding="utf-8").read())

    for name, text in HAND.items():
        cases.append({"name": f"hand-{name}", "source": "hand-written", "yaml": text})

    seen = set()
    for c in cases:
        assert c["name"] not in seen, c["name"]
        seen.add(c["name"])
    with open(out, "w", encoding="utf-8") as fh:
        json.dump(cases, fh, ensure_ascii=False, indent=1)
    print(f"{len(cases)} cases", file=sys.stderr)


if __name__ == "__main__":
    main(sys.argv[1])
