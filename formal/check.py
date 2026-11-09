#!/usr/bin/env python3
"""Model-check one model's cfgs, each of which must report no error.

Usage: python3 formal/check.py <tla2tools.jar> <model> pr|full

`pr` runs the model's pull-request gate, `full` its larger cfgs. Exits non-zero on the first cfg
that does not pass, printing TLC's output. A model's witness.py and mutants.py run separately.
"""
import os
import sys
import time

here = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, here)
from tlacheck import read, states, tlc  # noqa: E402

# model: (spec, whether TLC's deadlock check stays on, PR cfgs, full cfgs). The READMEs say why the
# deadlock check is on or off for each.
MODELS = {
    'lifecycle': ('Lifecycle', False, ['LifecyclePR'], ['Lifecycle', 'LifecycleApply', 'LifecycleReplace', 'LifecycleReplaceFail', 'Liveness']),
    'proxyonce': ('ProxyOnce', False, ['ProxyOncePR', 'ProxyOnceCarriers'],
                  ['ProxyOnce', 'ProxyOnceTwoEdits', 'ProxyOnceCarriers']),
    'scenario': ('Scenario', False, ['ScenarioPR'], ['Scenario']),
    'scripting': ('StateCell', True, ['StateCellPR', 'StateCellSequels'],
                  ['StateCell', 'Live', 'StateCellSequels']),
    'faultio': ('FaultIo', False, ['FaultIoPR'], ['FaultIo']),
}


def main():
    jar, model, mode = os.path.abspath(sys.argv[1]), sys.argv[2], sys.argv[3]
    spec_name, deadlock_on, pr_cfgs, full_cfgs = MODELS[model]
    directory = os.path.join(here, model)
    spec = read(os.path.join(directory, f'{spec_name}.tla'))
    for cfg_name in pr_cfgs if mode == 'pr' else full_cfgs:
        started = time.time()
        cfg = read(os.path.join(directory, f'{cfg_name}.cfg'))
        verdict, trace, out = tlc(jar, spec_name, spec, cfg, deadlock=not deadlock_on)
        print(f'{model}/{cfg_name}: {verdict}, (generated, distinct, depth) = {states(out)}, '
              f'{time.time() - started:.0f}s', flush=True)
        if verdict != 'ok':
            print(' -> '.join(trace))
            print(out)
            sys.exit(1)


if __name__ == '__main__':
    main()
