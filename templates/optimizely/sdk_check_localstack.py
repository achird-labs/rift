"""LocalStack-style check: the Optimizely Python SDK with ZERO code overrides.

The SDK dials https://cdn.optimizely.com, https://logx.optimizely.com and https://api.zaius.com
exactly as in production. Only the environment differs:

    OPTLY_ODP_HOST=https://api.zaius.com rift --configfile imposters.json &
    curl -s http://localhost:2525/intercept/ca.pem -o rift-ca.pem
    HTTPS_PROXY=http://127.0.0.1:4610 REQUESTS_CA_BUNDLE=$PWD/rift-ca.pem python3 sdk_check_localstack.py
"""
from __future__ import annotations

import json
import os
import sys

import requests
from optimizely import optimizely
from optimizely.helpers.sdk_settings import OptimizelySdkSettings

ADMIN = "http://localhost:2525"  # plain http: not routed through HTTPS_PROXY


def check(cond: bool, label: str) -> None:
    print(("  ok   " if cond else "  FAIL ") + label)
    if not cond:
        sys.exit(1)


def main() -> None:
    check(os.environ.get("HTTPS_PROXY", "").endswith(":4610"), "HTTPS_PROXY points at Rift's intercept listener")
    check(os.path.exists(os.environ.get("REQUESTS_CA_BUNDLE", "")), "REQUESTS_CA_BUNDLE points at the exported Rift CA")

    # Production wiring: just the SDK key. No url_template, no custom dispatcher.
    client = optimizely.Optimizely(sdk_key="MOCK_SDK_KEY", settings=OptimizelySdkSettings(odp_event_flush_interval=0))
    config = client.get_optimizely_config()
    check(config is not None and config.revision == "1", "datafile fetched from https://cdn.optimizely.com via the proxy")

    vip = client.create_user_context("user-vip")
    vip.fetch_qualified_segments()
    check(vip.get_qualified_segments() == ["high_value_customers"], "ODP segments fetched from https://api.zaius.com via the proxy")
    check(vip.decide("vip_support").enabled, "vip_support on for user-vip")

    premium = client.create_user_context("user-7", {"plan": "premium"})
    d = premium.decide("checkout_redesign")
    check(d.rule_key == "checkout_redesign_ab", f"premium user bucketed into {d.variation_key}")
    premium.track_event("checkout_completed")
    client.close()

    recorded = requests.get(f"{ADMIN}/imposters/4601/savedRequests", params={"match": "path=/v1/events"}, timeout=5).json()
    keys = sorted({ev["key"] for r in recorded for v in json.loads(r["body"])["visitors"] for s in v["snapshots"] for ev in s["events"]})
    check("checkout_completed" in keys, f"events posted to https://logx.optimizely.com landed in the mock: {keys}")
    cdn = requests.get(f"{ADMIN}/imposters/4600/savedRequests", timeout=5).json()
    # The forward rule delivers the host the SDK dialed, so recordings say which vendor host it was.
    sdk_fetches = [r for r in cdn if r["headers"].get("User-Agent", "").startswith("python-requests")]
    check(bool(sdk_fetches), f"CDN imposter recorded {len(sdk_fetches)} datafile fetch(es) from the SDK's HTTP client")
    check(all(r["headers"].get("Host") == "cdn.optimizely.com" for r in sdk_fetches),
          "each recorded fetch carries the dialed host (Host: cdn.optimizely.com)")
    print("localstack-mode check passed")


if __name__ == "__main__":
    main()
