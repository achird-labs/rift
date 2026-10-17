# Docs visuals

Animated SVGs for the README and the docs site, drawn by small Python scripts (stdlib only), plus
the logo PNGs.

```sh
python3 scripts/visuals/build.py              # every scene -> docs/assets/<name>.svg
python3 scripts/visuals/build.py matching     # one scene
scripts/visuals/check.sh                      # what CI runs: rebuild to a temp dir and diff
```

| Scene | Script | Shows |
|:------|:-------|:------|
| `hero` | `hero.py` | first match answers from its cycle; a proxyOnce proxy records, then replays |
| `response-cycle` | `response_cycle.py` | a stub's responses as a rotating queue, `repeat`, what resets the cycle |
| `proxy-modes` | `proxy_modes.py` | proxyOnce, proxyAlways, proxyTransparent: what is recorded, where, what replays |
| `matching` | `matching.py` | predicates AND-ed and short-circuited, stubs in order, the defaultResponse fallback |
| `connection-path` | `connection_path.py` | accept, `--local-only`, tuning, FaultIo, TLS, HTTP, match, behaviors |
| `scenarios` | `scenarios.py` | `requiredScenarioState` gating and `newScenarioState` transitions |

Each scene script starts with a docstring naming the docs section and engine code it depicts, and
what was probed against a running engine. A scene must not show behaviour the engine does not have:
when the engine changes, update the scene and its docstring.

## How it works

`hud.py` draws the dark frame and turns timed keyframes into CSS. Every element in a scene moves on
one shared loop of `T` seconds; `Scene.path` moves a group through `(t, dx, dy[, opacity])` points
and `Scene.shown` fades it in and out over time windows. There is no JavaScript, so the files play
inside a README `<img>`. Under `prefers-reduced-motion: reduce` the animation stops on the frame at
`Scene.still`, which each scene sets to its end state. Each SVG carries an `aria-label` describing
what it shows, and the output is deterministic, so `check.sh` can diff it.

Limits checked in CI: valid XML, an `aria-label`, at most 40 KB.

## Logo

`logo.py` derives `logo.png`, `logo-dark.png`, `logo-mark.png` and `favicon.png` from the owner's
source image. It needs Pillow, so it is not part of `check.sh`; run it in a throwaway venv:

```sh
python3 -m venv /tmp/img && /tmp/img/bin/pip install pillow
/tmp/img/bin/python scripts/visuals/logo.py "path/to/Rift Gradient Tech Logo.png"
```

Palette, sampled from the logo and shared with `hud.py`: navy `#0e0f1d`, cyan `#53cafd`, blue
`#5b87fd`, indigo `#6260f9`, violet `#8e41fd`. Green `#34d399`, red `#f87171` and amber `#fbbf24` are
kept for states (served, refused, reset).
