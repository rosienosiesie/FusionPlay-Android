#!/usr/bin/env python3
"""Summarize MiPlay DNS-SD identities from probe_miplay_mdns JSONL captures."""

from __future__ import annotations

import argparse
import collections
import json
from pathlib import Path


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("captures", nargs="+", type=Path)
    args = parser.parse_args()

    for capture in args.captures:
        ptrs: dict[tuple[str, str, str], list[tuple[str, int]]] = collections.defaultdict(list)
        txts: dict[tuple[str, str], set[tuple[str, ...]]] = collections.defaultdict(set)
        srvs: dict[tuple[str, str], set[str]] = collections.defaultdict(set)
        packets = 0
        with capture.open(encoding="utf-8") as source:
            for line in source:
                row = json.loads(line)
                packets += 1
                source_ip = row["source"]["ip"]
                timestamp = row["timestamp_utc"]
                for record in row.get("dns", {}).get("records", []):
                    name = str(record["name"])
                    key = (source_ip, name)
                    if record["type"] == 12:
                        ptrs[(source_ip, name, str(record["value"]))].append(
                            (timestamp, int(record["ttl"]))
                        )
                    elif record["type"] == 16:
                        txts[key].add(tuple(str(value) for value in record["value"]))
                    elif record["type"] == 33:
                        srvs[key].add(json.dumps(record["value"], ensure_ascii=False, sort_keys=True))

        print(f"=== {capture} ({packets} packets) ===")
        for (source_ip, service, instance), observations in sorted(ptrs.items()):
            ttls = sorted({ttl for _, ttl in observations})
            first = observations[0][0]
            last = observations[-1][0]
            print(
                f"PTR source={source_ip} service={service} instance={instance} "
                f"ttls={ttls} count={len(observations)} first={first} last={last}"
            )
            identity_key = (source_ip, instance)
            for values in sorted(txts.get(identity_key, set())):
                print(f"  TXT {list(values)}")
            for value in sorted(srvs.get(identity_key, set())):
                print(f"  SRV {value}")
        print()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
