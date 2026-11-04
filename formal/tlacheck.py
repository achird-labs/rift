"""Shared by every model's witness.py, mutants.py and validate.py: run TLC on a copy of a spec in a
scratch directory and read its verdict.

Usage from a model directory:
    from tlacheck import edit, tlc, jar_from_argv
    verdict, trace, out = tlc(jar, 'Lifecycle', spec_text, cfg_text)

`verdict` is the name of the violated invariant or property ('temporal' when TLC does not name it), 'deadlock', 'ok' (model checking completed with no
error), or 'error' (TLC failed; `out` says why).  `trace` is the counterexample as a list of
"Action(args)" strings, empty when there is none.
"""
import os
import re
import subprocess
import sys
import tempfile


def jar_from_argv():
    """The tla2tools.jar path: argv[1], else $TLA_TOOLS_JAR, else ./tla2tools.jar."""
    path = sys.argv[1] if len(sys.argv) > 1 else os.environ.get('TLA_TOOLS_JAR', 'tla2tools.jar')
    path = os.path.abspath(path)
    if not os.path.exists(path):
        raise SystemExit(f'no tla2tools.jar at {path}; pass its path or set TLA_TOOLS_JAR')
    return path


def read(path):
    with open(path) as f:
        return f.read()


def edit(text, edits, what):
    """Apply (old, new) replacements, each of which must match exactly once; else the edit is stale."""
    for old, new in edits:
        if text.count(old) != 1:
            raise SystemExit(f'{what}: the edit "{old[:60]}..." applies {text.count(old)} times, not once; update it')
        text = text.replace(old, new)
    return text


def tlc(jar, module, spec, cfg, extra_modules=None, workers='auto', timeout=None, deadlock=True, java_opts=()):
    """Run TLC on `spec` (the text of <module>.tla) with `cfg` (the text of its .cfg)."""
    with tempfile.TemporaryDirectory() as d:
        with open(os.path.join(d, f'{module}.tla'), 'w') as f:
            f.write(spec)
        with open(os.path.join(d, f'{module}.cfg'), 'w') as f:
            f.write(cfg)
        for name, text in (extra_modules or {}).items():
            with open(os.path.join(d, f'{name}.tla'), 'w') as f:
                f.write(text)
        cmd = ['java', '-XX:+UseParallelGC', *java_opts, '-cp', jar, 'tlc2.TLC', '-workers', str(workers),
               '-config', f'{module}.cfg', f'{module}.tla']
        if deadlock:
            cmd.insert(cmd.index('-config'), '-deadlock')
        try:
            run = subprocess.run(cmd, cwd=d, capture_output=True, text=True, timeout=timeout)
        except subprocess.TimeoutExpired:
            return 'timeout', [], ''
        out = run.stdout + run.stderr
    found = re.findall(r'Invariant (\w+) is violated|Temporal propert(?:y|ies) (\w+) was violated|(Deadlock) reached',
                       out)
    names = [x.lower() if x == 'Deadlock' else x for t in found for x in t if x]
    # TLC 1.7.4, which CI pins, does not name the temporal property it reports.
    if not names and 'Temporal properties were violated' in out:
        names = ['temporal']
    trace = re.findall(r'^State \d+: <(\w+)(\([^)]*\))? line', out, re.M)
    trace = [a + (args or '') for a, args in trace]
    if names:
        return names[0], trace, out[-2500:]
    if 'Model checking completed. No error has been found.' in out:
        return 'ok', [], out[-2500:]
    return 'error', [], out[-3000:]


def states(out):
    """(generated, distinct, depth) from TLC's summary lines, or None."""
    m = re.search(r'(\d+) states generated, (\d+) distinct states found', out)
    d = re.search(r'depth of the complete state graph search is (\d+)', out)
    return (int(m.group(1)), int(m.group(2)), int(d.group(1)) if d else None) if m else None


def without_invariant(cfg, name):
    """The cfg without `name` in its INVARIANTS block."""
    lines = [l for l in cfg.splitlines() if l.split('\\*')[0].strip() != name]
    return '\n'.join(lines) + '\n'
