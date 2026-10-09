#!/usr/bin/env python3
"""Fail closed unless every prerequisite CI job succeeded.

Reads `NEEDS_JSON` (the `needs` context of the CI Gate job). Skipped, cancelled
or missing jobs count as failures, so branch protection can require only this gate.
"""

import json
import os
import sys

# Must equal the `needs:` list of the `gate` job in .github/workflows/ci.yml.
REQUIRED_JOBS = frozenset(("test", "quality", "release", "audit", "e2e", "copilot"))


def failures(raw: str) -> list[str]:
    """Return one line per unsuccessful job; raise ValueError for malformed input."""
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


def main() -> int:
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
