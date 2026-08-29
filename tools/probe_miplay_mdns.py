#!/usr/bin/env python3
"""Probe and decode FusionPlay's Xiaomi MiPlay mDNS advertisements."""

from __future__ import annotations

import argparse
import datetime as dt
import json
import socket
import struct
import time
from pathlib import Path


MDNS_ADDRESS = "224.0.0.251"
MDNS_PORT = 5353
SERVICES = ("_lyra-mdns._udp.local.", "_mi-connect._udp.local.")


def encode_name(name: str) -> bytes:
    return b"".join(bytes((len(part),)) + part.encode("utf-8") for part in name.rstrip(".").split(".")) + b"\0"


def query(name: str) -> bytes:
    return struct.pack("!6H", 0, 0, 1, 0, 0, 0) + encode_name(name) + struct.pack("!HH", 12, 1)


def read_name(packet: bytes, offset: int, visited: set[int] | None = None) -> tuple[str, int]:
    labels: list[str] = []
    cursor = offset
    end = None
    visited = set() if visited is None else visited
    while True:
        if cursor >= len(packet):
            raise ValueError("truncated DNS name")
        length = packet[cursor]
        if length & 0xC0 == 0xC0:
            if cursor + 1 >= len(packet):
                raise ValueError("truncated DNS pointer")
            pointer = ((length & 0x3F) << 8) | packet[cursor + 1]
            if pointer in visited:
                raise ValueError("DNS pointer cycle")
            visited.add(pointer)
            if end is None:
                end = cursor + 2
            nested, _ = read_name(packet, pointer, visited)
            labels.extend(nested.rstrip(".").split("."))
            cursor += 2
            break
        cursor += 1
        if length == 0:
            break
        if cursor + length > len(packet):
            raise ValueError("truncated DNS label")
        labels.append(packet[cursor : cursor + length].decode("utf-8", errors="replace"))
        cursor += length
    return ".".join(labels) + ".", end if end is not None else cursor


def decode_txt(data: bytes) -> list[str]:
    values: list[str] = []
    cursor = 0
    while cursor < len(data):
        length = data[cursor]
        cursor += 1
        values.append(data[cursor : cursor + length].decode("utf-8", errors="replace"))
        cursor += length
    return values


def decode_packet(packet: bytes) -> dict[str, object]:
    if len(packet) < 12:
        raise ValueError("short DNS packet")
    transaction_id, flags, qd, an, ns, ar = struct.unpack_from("!6H", packet)
    cursor = 12
    questions = []
    for _ in range(qd):
        name, cursor = read_name(packet, cursor)
        qtype, qclass = struct.unpack_from("!HH", packet, cursor)
        cursor += 4
        questions.append({"name": name, "type": qtype, "class": qclass})
    records = []
    for section, count in (("answer", an), ("authority", ns), ("additional", ar)):
        for _ in range(count):
            name, cursor = read_name(packet, cursor)
            rtype, rclass, ttl, length = struct.unpack_from("!HHIH", packet, cursor)
            cursor += 10
            data_offset = cursor
            data = packet[cursor : cursor + length]
            cursor += length
            value: object = data.hex()
            if rtype in (12, 5, 2):
                value, _ = read_name(packet, data_offset)
            elif rtype == 16:
                value = decode_txt(data)
            elif rtype == 1 and len(data) == 4:
                value = socket.inet_ntoa(data)
            elif rtype == 28 and len(data) == 16:
                value = socket.inet_ntop(socket.AF_INET6, data)
            elif rtype == 33 and len(data) >= 6:
                priority, weight, port = struct.unpack_from("!HHH", data)
                target, _ = read_name(packet, data_offset + 6)
                value = {"priority": priority, "weight": weight, "port": port, "target": target}
            records.append({
                "section": section,
                "name": name,
                "type": rtype,
                "class": rclass,
                "ttl": ttl,
                "value": value,
            })
    return {
        "id": transaction_id,
        "flags": flags,
        "questions": questions,
        "records": records,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--interface", required=True)
    parser.add_argument("--target", default=None)
    parser.add_argument("--seconds", type=float, default=8.0)
    parser.add_argument("--passive", action="store_true")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM, socket.IPPROTO_UDP)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    sock.bind(("", MDNS_PORT))
    membership = socket.inet_aton(MDNS_ADDRESS) + socket.inet_aton(args.interface)
    sock.setsockopt(socket.IPPROTO_IP, socket.IP_ADD_MEMBERSHIP, membership)
    sock.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_IF, socket.inet_aton(args.interface))
    sock.settimeout(0.25)

    args.output.parent.mkdir(parents=True, exist_ok=True)
    deadline = time.monotonic() + args.seconds
    next_probe = 0.0
    seen = 0
    with args.output.open("w", encoding="utf-8") as output:
        while time.monotonic() < deadline:
            now = time.monotonic()
            if not args.passive and now >= next_probe:
                for service in SERVICES:
                    sock.sendto(query(service), (MDNS_ADDRESS, MDNS_PORT))
                next_probe = now + 1.0
            try:
                packet, source = sock.recvfrom(65535)
            except TimeoutError:
                continue
            if args.target and source[0] != args.target:
                continue
            try:
                decoded = decode_packet(packet)
            except Exception as error:
                decoded = {"decode_error": str(error)}
            row = {
                "timestamp_utc": dt.datetime.now(dt.timezone.utc).isoformat(),
                "source": {"ip": source[0], "port": source[1]},
                "length": len(packet),
                "dns": decoded,
                "wire_hex": packet.hex(),
            }
            output.write(json.dumps(row, ensure_ascii=False) + "\n")
            output.flush()
            seen += 1
    print(json.dumps({"packets": seen, "output": str(args.output)}, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
