"""Validate and query OpenZFS ``-j`` JSON output for the E2E scripts.

Usage: zfs_json.py TOOL COMMAND MODE [ARGS...] < document

TOOL is ``zpool`` or ``zfs``; COMMAND is informational. Modes:
  online           print the names of pools whose health and state are ONLINE
  absent NAME      fail if NAME is present
  value NAME PROP  print one validated property value

Only the JSON machine interface is read; display tables are never parsed. Any
structural problem exits non-zero with ``invalid OpenZFS JSON: ...``.
"""

from __future__ import annotations

import json
import re
import sys

MAX_GUID = 2**64 - 1
POOL_TYPES = ("POOL",)
DATASET_TYPES = ("FILESYSTEM", "SNAPSHOT", "VOLUME")


def named_objects(tool: str, document: dict) -> dict:
    """Return the pool or dataset map after checking every entry's name and type."""
    objects = document["pools" if tool == "zpool" else "datasets"]
    if not isinstance(objects, dict):
        raise ValueError("expected a named object map")
    allowed = POOL_TYPES if tool == "zpool" else DATASET_TYPES
    for name, item in objects.items():
        if not name or not isinstance(item, dict) or item.get("name") != name:
            raise ValueError("object map key/name mismatch")
        if item.get("type") not in allowed:
            raise ValueError("invalid object type")
    return objects


def property_value(objects: dict, name: str, prop: str) -> str:
    raw = objects[name]["properties"][prop]["value"]
    if not isinstance(raw, str) or not raw or "\n" in raw or "\r" in raw:
        raise ValueError("missing, empty or malformed property " + prop)
    if prop == "guid" and (not re.fullmatch(r"[1-9][0-9]*", raw) or int(raw) > MAX_GUID):
        raise ValueError("invalid decimal GUID")
    return raw


def query(tool: str, mode: str, args: list[str], document: dict) -> list[str]:
    """Evaluate one reader mode and return the lines to print."""
    objects = named_objects(tool, document)
    if mode == "online":
        online = []
        for name, item in objects.items():
            health = property_value(objects, name, "health")
            if not isinstance(item.get("state"), str) or not item["state"]:
                raise ValueError("missing pool state")
            if health == "ONLINE" and item["state"] == "ONLINE":
                online.append(name)
        return online
    if mode == "absent":
        if args[0] in objects:
            raise ValueError("test namespace already exists: " + args[0])
        return []
    if mode == "value":
        if tool == "zfs":
            expected = "SNAPSHOT" if "@" in args[0] else "FILESYSTEM"
            if objects[args[0]]["type"] != expected:
                raise ValueError("unexpected requested dataset type")
        return [property_value(objects, *args)]
    raise ValueError("unknown JSON reader mode")


def main(argv: list[str]) -> None:
    try:
        tool, _command, mode, *args = argv
        for line in query(tool, mode, args, json.load(sys.stdin)):
            print(line)
    except (ValueError, KeyError, TypeError, IndexError) as error:
        sys.exit("invalid OpenZFS JSON: " + str(error))


if __name__ == "__main__":
    main(sys.argv[1:])
