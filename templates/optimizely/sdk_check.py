"""Drive the real Optimizely Python SDK against the Rift mock.

    pip install optimizely-sdk requests
    rift --configfile imposters.json &
    python3 sdk_check.py

Only two things differ from a production wiring: the datafile URL template and the
host inside the event dispatcher. ODP needs no override because the datafile's
`integrations[odp].host` already points at the mock.
"""
from __future__ import annotations

import json
import sys

import requests
from optimizely import optimizely
from optimizely.config_manager import PollingConfigManager
from optimizely.event_dispatcher import EventDispatcher
from optimizely.helpers.sdk_settings import OptimizelySdkSettings

CDN = "http://localhost:4600"
LOGX = "http://localhost:4601"
ADMIN = "http://localhost:2525"


class MockEventDispatcher(EventDispatcher):
    """Same as the stock dispatcher, but aimed at the mock instead of logx.optimizely.com."""

    @staticmethod
    def dispatch_event(event) -> None:
        url = event.url.replace("https://logx.optimizely.com", LOGX)
        resp = requests.post(url, data=json.dumps(event.params), headers=event.headers, timeout=5)
        if resp.status_code != 204:
            raise RuntimeError(f"event dispatch failed: {resp.status_code} {resp.text}")


def check(cond: bool, label: str) -> None:
    print(("  ok   " if cond else "  FAIL ") + label)
    if not cond:
        sys.exit(1)


def main() -> None:
    config_manager = PollingConfigManager(
        sdk_key="MOCK_SDK_KEY",
        url_template=CDN + "/datafiles/{sdk_key}.json",
        update_interval=30,
        blocking_timeout=5,
    )
    client = optimizely.Optimizely(
        config_manager=config_manager,
        event_dispatcher=MockEventDispatcher,
        settings=OptimizelySdkSettings(odp_disabled=False, odp_event_flush_interval=0),
    )
    config = client.get_optimizely_config()
    check(config is not None and config.revision == "1", "SDK fetched datafile revision 1 from the mock CDN")
    check(set(config.features_map) == {"checkout_redesign", "vip_support", "legacy_search"}, "flags visible through OptimizelyConfig")

    # Rollout to everyone: enabled, default variable.
    anon = client.create_user_context("user-42", {"plan": "free"})
    d = anon.decide("checkout_redesign")
    check(d.enabled and d.variables["layout"] == "classic" and d.rule_key == "checkout_redesign_everyone",
          "free user: checkout_redesign on via rollout, layout=classic")

    # A/B experiment gated on the 'plan == premium' audience: bucketed into control or treatment.
    premium = client.create_user_context("user-7", {"plan": "premium"})
    d = premium.decide("checkout_redesign")
    check(d.enabled and d.rule_key == "checkout_redesign_ab" and d.variation_key in {"control", "treatment"},
          f"premium user: bucketed by the SDK into {d.variation_key} of checkout_redesign_ab")

    # Kill switch: everyone off.
    check(not anon.decide("legacy_search").enabled, "legacy_search off for everyone")

    # ODP real-time segment: the SDK calls POST /v3/graphql on the host from the datafile.
    vip = client.create_user_context("user-vip")
    check(vip.fetch_qualified_segments(), "fetch_qualified_segments returned")
    check(vip.get_qualified_segments() == ["high_value_customers"], "user-vip qualifies for high_value_customers")
    check(vip.decide("vip_support").enabled, "vip_support on for user-vip (ODP-targeted rule)")
    other = client.create_user_context("user-42")
    other.fetch_qualified_segments()
    check(other.get_qualified_segments() == [] and not other.decide("vip_support").enabled,
          "vip_support off for a user with no segments")

    # Conversion event -> POST /v1/events on the mock.
    premium.track_event("checkout_completed", {"revenue": 4200})
    client.close()

    # Assert on what the mock recorded, through Rift's admin API.
    recorded = requests.get(f"{ADMIN}/imposters/4601/savedRequests", params={"match": "path=/v1/events"}, timeout=5).json()
    keys = sorted({ev["key"] for r in recorded for v in json.loads(r["body"])["visitors"] for s in v["snapshots"] for ev in s["events"]})
    check("checkout_completed" in keys, f"mock recorded event batches with keys {keys}")
    verify = requests.post(f"{ADMIN}/imposters/4602/verify", json={"predicates": [{"equals": {"path": "/v3/graphql"}}]}, timeout=5).json()
    check(verify["matched"] >= 2, f"mock saw {verify['matched']} ODP segment lookups")
    print("sdk check passed")


if __name__ == "__main__":
    main()
