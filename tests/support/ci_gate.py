#!/usr/bin/env python3
import json
import os
import sys


REQUIRED_JOBS = frozenset(("test", "quality", "release", "audit", "e2e"))


def failures(raw):
    jobs = json.loads(raw)
    if not isinstance(jobs, dict) or set(jobs) != REQUIRED_JOBS:
        raise ValueError("CI Gate requires exactly all configured prerequisite jobs")
    failed = []
    for name, job in sorted(jobs.items()):
        if not isinstance(job, dict):
            raise ValueError(f"{name}: invalid job result")
        if job.get("result") != "success":
            failed.append(f"{name}: {job.get('result', 'missing result')}")
    return failed


def main():
    try:
        failed = failures(os.environ["NEEDS_JSON"])
    except (KeyError, TypeError, ValueError) as error:
        print(f"CI Gate rejected invalid job results: {error}", file=sys.stderr)
        return 1
    if failed:
        print("CI Gate failed:\n" + "\n".join(failed), file=sys.stderr)
        return 1
    print("CI Gate passed: every prerequisite job succeeded")
    return 0


if __name__ == "__main__":
    sys.exit(main())
