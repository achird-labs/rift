"""Writes the animated SVGs to docs/assets.

    python3 scripts/visuals/build.py [--out DIR] [name ...]

With no names it writes every scene. `--out` writes elsewhere (check.sh uses it to diff against the
committed files).
"""

import importlib
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

SCENES = {
    "hero": "hero",
    "response-cycle": "response_cycle",
    "proxy-modes": "proxy_modes",
    "matching": "matching",
    "connection-path": "connection_path",
    "scenarios": "scenarios",
}


def main(argv: list[str]) -> int:
    out = os.path.join(HERE, "..", "..", "docs", "assets")
    if argv[:1] == ["--out"]:
        out, argv = argv[1], argv[2:]
    unknown = [n for n in argv if n not in SCENES]
    if unknown:
        print(f"unknown scene(s): {', '.join(unknown)}; known: {', '.join(SCENES)}", file=sys.stderr)
        return 2
    os.makedirs(out, exist_ok=True)
    for name in argv or list(SCENES):
        scene = importlib.import_module(SCENES[name]).build()
        path = os.path.join(out, f"{name}.svg")
        scene.write(path)
        print(f"{path} ({os.path.getsize(path)} bytes)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
