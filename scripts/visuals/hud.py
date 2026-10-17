"""A small, dependency-free toolkit for Rift's animated docs SVGs.

A `Scene` is a dark HUD frame (title, status line, footer) plus elements that move on CSS keyframes.
Every animation in a scene shares one loop of `T` seconds, and times are seconds into that loop.

An animated element is drawn at its resting coordinates and wrapped in a group whose class plays a
keyframe animation over a few properties: `o` (opacity), `x`/`y` (a translation in px) and `s`
(scale). Under `prefers-reduced-motion: reduce` every animation stops and each group is pinned to
its state at `Scene.still`, a moment in the loop chosen as the scene's meaningful end state.

There is no JavaScript, so the SVG plays in a README `<img>` as well as on the docs site. The output
is a pure function of the scene script, so rebuilding gives byte-identical files.
"""

from xml.sax.saxutils import escape, quoteattr

MONO = "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace"

# Brand accents, sampled from the owner's logo: a cyan -> indigo -> violet gradient on navy.
NAVY = "#0e0f1d"
CYAN, BLUE, INDIGO, VIOLET = "#53cafd", "#5b87fd", "#6260f9", "#8e41fd"
# Semantic states only: served / matched, refused / failed, warning / reset.
GREEN, RED, AMBER = "#34d399", "#f87171", "#fbbf24"
# Neutrals, tinted toward the navy.
TEXT, DIM, FAINT, EDGE, LINE = "#e6e8f5", "#8085a8", "#5a5f80", "#2c2f4d", "#1b1d33"

SOLID = "#15172b"  # an opaque backing, so an overlay hides what it replaces


def num(v: float) -> str:
    """A compact, deterministic number: at most two decimals, no trailing zeros."""
    s = f"{v:.2f}".rstrip("0").rstrip(".")
    return "0" if s in ("-0", "") else s


def css_of(p: dict) -> str:
    out = []
    if "o" in p:
        out.append(f"opacity:{num(p['o'])}")
    if "x" in p or "y" in p or "s" in p:
        t = f"translate({num(p.get('x', 0))}px,{num(p.get('y', 0))}px)"
        if "s" in p:
            t += f" scale({num(p['s'])})"
        out.append(f"transform:{t}")
    return ";".join(out)


class Scene:
    def __init__(self, w: int, h: int, T: float, title: str, hud: str, footer: str, label: str,
                 still: float | None = None):
        self.w, self.h, self.T = w, h, T
        self.title, self.hud, self.footer, self.label = title, hud, footer, label
        self.still = T - 0.6 if still is None else still
        self.css: list[str] = []
        self.rest: list[str] = []
        self.body: list[str] = []
        self.uid = 0

    # --- timing -----------------------------------------------------------------------------------

    def _pct(self, t: float) -> str:
        return num(max(0.0, min(100.0, t / self.T * 100))) + "%"

    def _at(self, frames: list[tuple[float, dict]], t: float) -> dict:
        """The interpolated properties at time `t` (linear, which is what a still frame needs)."""
        before = [f for f in frames if f[0] <= t]
        after = [f for f in frames if f[0] > t]
        if not before:
            return dict(after[0][1])
        t0, p0 = before[-1]
        if not after:
            return dict(p0)
        t1, p1 = after[0]
        k = (t - t0) / (t1 - t0) if t1 > t0 else 0.0
        return {key: p0[key] + (p1.get(key, p0[key]) - p0[key]) * k for key in p0}

    def anim(self, frames: list[tuple[float, dict]], ease: str = "linear") -> str:
        """Registers keyframes `(time, props)`; returns the class that plays them.

        Frames are padded to 0 and T with the first and last states, so an element holds still
        outside the times given. Every frame must carry the same property keys.
        """
        frames = sorted(frames, key=lambda f: f[0])
        frames = [(0.0, frames[0][1])] + frames + [(self.T, frames[-1][1])]
        self.uid += 1
        name = f"a{self.uid}"
        steps, last = [], None
        for t, p in frames:
            pct = self._pct(t)
            if pct == last:  # two frames on one percentage: the later one wins
                steps[-1] = f"{pct}{{{css_of(p)}}}"
            else:
                steps.append(f"{pct}{{{css_of(p)}}}")
            last = pct
        self.css.append(f".{name}{{animation:{name} {num(self.T)}s {ease} infinite}}"
                        f"@keyframes {name}{{{''.join(steps)}}}")
        self.rest.append(f".{name}{{animation:none;{css_of(self._at(frames, self.still))}}}")
        return name

    def shown(self, windows: list[tuple[float, float]], fade: float = 0.15) -> str:
        """Visible during each `(on, off)` window, fading in and out. A window may start at 0 or end
        at T to be visible across the loop's seam."""
        frames: list[tuple[float, dict]] = []
        for on, off in sorted(windows):
            frames += [(on, {"o": 0 if on > 0 else 1}), (min(on + fade, off), {"o": 1}),
                       (max(off - fade, on), {"o": 1}), (off, {"o": 0 if off < self.T else 1})]
        if frames[0][0] > 0:
            frames.insert(0, (0.0, {"o": 0}))
        return self.anim(frames)

    def path(self, points: list[tuple], ease: str = "ease-in-out") -> str:
        """Moves a group through `(t, dx, dy[, opacity])` offsets from where it is drawn."""
        pts = sorted(p if len(p) == 4 else (*p, 1.0) for p in points)
        return self.anim([(t, {"o": o, "x": x, "y": y}) for t, x, y, o in pts], ease)

    # --- drawing ----------------------------------------------------------------------------------

    def add(self, *parts: str) -> None:
        self.body.extend(parts)

    @staticmethod
    def g(cls: str, *parts: str) -> str:
        return f'<g class="{cls}">{"".join(parts)}</g>'

    @staticmethod
    def text(x, y, s, cls="hud", fill=None, anchor="start", weight=None) -> str:
        f = f' fill="{fill}"' if fill else ""
        a = f' text-anchor="{anchor}"' if anchor != "start" else ""
        wgt = f' font-weight="{weight}"' if weight else ""
        return f'<text x="{num(x)}" y="{num(y)}" class="{cls}"{a}{f}{wgt}>{escape(s)}</text>'

    @staticmethod
    def width(s: str, size: float = 10.5) -> float:
        """Approximate monospace advance width."""
        return len(s) * size * 0.62

    def chip(self, x, y, s, color, size=10.5, pad=8, h=20, glow=True) -> str:
        """A labelled pill centred at (x, y)."""
        w = self.width(s, size) + 2 * pad
        f = ' filter="url(#glow)"' if glow else ""
        return (f'<rect x="{num(x - w / 2)}" y="{num(y - h / 2)}" width="{num(w)}" height="{h}" rx="5" '
                f'fill="{color}" fill-opacity=".16" stroke="{color}" stroke-width="1.4"{f}/>'
                f'<text x="{num(x)}" y="{num(y + size * 0.36)}" class="chip" text-anchor="middle" '
                f'fill="{color}" style="font-size:{num(size)}px">{escape(s)}</text>')

    def badge(self, x, y, s, color, size=10.5) -> str:
        """A quiet tag whose left edge is at x."""
        w = self.width(s, size) + 14
        return (f'<rect x="{num(x)}" y="{num(y - 10)}" width="{num(w)}" height="19" rx="4" fill="{SOLID}"/>'
                f'<rect x="{num(x)}" y="{num(y - 10)}" width="{num(w)}" height="19" rx="4" fill="{color}" '
                f'fill-opacity=".1" stroke="{color}" stroke-opacity=".55"/>'
                f'<text x="{num(x + w / 2)}" y="{num(y + 3.5)}" class="chip" text-anchor="middle" '
                f'fill="{color}" style="font-size:{num(size)}px">{escape(s)}</text>')

    @staticmethod
    def panel(x, y, w, h, fill="url(#panel)", stroke=EDGE, rx=10) -> str:
        return (f'<rect x="{num(x)}" y="{num(y)}" width="{num(w)}" height="{num(h)}" rx="{rx}" '
                f'fill="{fill}" stroke="{stroke}"/>')

    @staticmethod
    def box(x, y, w, h, color, opacity=".12", rx=6, dash=False) -> str:
        d = ' stroke-dasharray="4 4"' if dash else ""
        return (f'<rect x="{num(x)}" y="{num(y)}" width="{num(w)}" height="{num(h)}" rx="{rx}" '
                f'fill="{color}" fill-opacity="{opacity}" stroke="{color}" stroke-width="1.4"{d}/>')

    @staticmethod
    def rail(x1, y1, x2, y2, color) -> str:
        """A track with dashes flowing from (x1, y1) toward (x2, y2)."""
        c = f'x1="{num(x1)}" y1="{num(y1)}" x2="{num(x2)}" y2="{num(y2)}"'
        return f'<line {c} class="rail"/><line {c} class="flow" stroke="{color}"/>'

    @staticmethod
    def line(x1, y1, x2, y2, color=EDGE, width=1.4, dash=None) -> str:
        d = f' stroke-dasharray="{dash}"' if dash else ""
        return (f'<line x1="{num(x1)}" y1="{num(y1)}" x2="{num(x2)}" y2="{num(y2)}" stroke="{color}" '
                f'stroke-width="{num(width)}"{d}/>')

    @staticmethod
    def arrow(x1, y1, x2, y2, color=DIM) -> str:
        return (f'<line x1="{num(x1)}" y1="{num(y1)}" x2="{num(x2)}" y2="{num(y2)}" stroke="{color}" '
                f'stroke-width="1.4" marker-end="url(#tip)" style="color:{color}"/>')

    def pulse(self, x, y, color, at: float, r=12) -> str:
        cls = self.anim([(0.0, {"o": 0, "s": 0.4}), (at, {"o": 0, "s": 0.4}), (at + 0.05, {"o": 0.9, "s": 0.4}),
                         (at + 0.7, {"o": 0, "s": 2.2})])
        return (f'<circle cx="{num(x)}" cy="{num(y)}" r="{r}" fill="none" stroke="{color}" stroke-width="2" '
                f'class="pulse {cls}"/>')

    # --- output -----------------------------------------------------------------------------------

    def svg(self) -> str:
        W, H = self.w, self.h
        return f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="{W}" height="{H}" role="img" aria-label={quoteattr(self.label)}>
<title>{escape(self.label)}</title>
<defs>
<filter id="glow" x="-50%" y="-50%" width="200%" height="200%"><feGaussianBlur stdDeviation="2.2" result="b"/><feMerge><feMergeNode in="b"/><feMergeNode in="SourceGraphic"/></feMerge></filter>
<pattern id="grid" width="24" height="24" patternUnits="userSpaceOnUse"><path d="M24 0H0V24" fill="none" stroke="{LINE}"/></pattern>
<linearGradient id="bg" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="#090a15"/><stop offset="1" stop-color="{NAVY}"/></linearGradient>
<linearGradient id="panel" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#191b33"/><stop offset="1" stop-color="#10111f"/></linearGradient>
<linearGradient id="brand" x1="0" y1="0" x2="1" y2="0"><stop offset="0" stop-color="{CYAN}"/><stop offset=".5" stop-color="{INDIGO}"/><stop offset="1" stop-color="{VIOLET}"/></linearGradient>
<marker id="tip" viewBox="0 0 8 8" refX="7" refY="4" markerWidth="7" markerHeight="7" orient="auto-start-reverse"><path d="M0 0L8 4L0 8z" fill="currentColor"/></marker>
</defs>
<style>
text{{font-family:{MONO}}}
.title{{font-size:15px;font-weight:700;fill:{TEXT};letter-spacing:.5px}}
.hud{{font-size:11.5px;fill:{DIM}}}
.tiny{{font-size:10px;fill:{FAINT};letter-spacing:1px}}
.cap{{font-size:11px;fill:{DIM}}}
.lbl{{font-size:12px;fill:{TEXT}}}
.big{{font-size:13px;font-weight:700;fill:{TEXT}}}
.chip{{font-weight:700}}
.mark{{font-size:13px;font-weight:700}}
.rail{{stroke:{LINE};stroke-width:3}}
.flow{{stroke-width:1.5;stroke-dasharray:2 10;stroke-opacity:.6;animation:flow 1s linear infinite}}
.pulse{{transform-box:fill-box;transform-origin:center}}
@keyframes flow{{to{{stroke-dashoffset:-12}}}}
{chr(10).join(self.css)}
@media (prefers-reduced-motion:reduce){{
.flow{{animation:none}}
{chr(10).join(self.rest)}
}}
</style>
<rect width="{W}" height="{H}" rx="14" fill="url(#bg)"/>
<rect width="{W}" height="{H}" rx="14" fill="url(#grid)" opacity=".5"/>
<rect x=".75" y=".75" width="{W - 1.5}" height="{H - 1.5}" rx="14" fill="none" stroke="#262a52"/>
<rect x="28" y="26" width="4" height="18" rx="2" fill="url(#brand)"/>
<text x="40" y="40" class="title">{escape(self.title)}</text>
<text x="{W - 28}" y="40" class="hud" text-anchor="end">{escape(self.hud)}</text>
<line x1="28" y1="58" x2="{W - 28}" y2="58" stroke="{LINE}"/>
{"".join(self.body)}
<line x1="28" y1="{H - 46}" x2="{W - 28}" y2="{H - 46}" stroke="{LINE}"/>
<text x="28" y="{H - 21}" class="hud">{escape(self.footer)}</text>
</svg>
"""

    def write(self, path: str) -> None:
        with open(path, "w", encoding="utf-8", newline="\n") as f:
            f.write(self.svg())
