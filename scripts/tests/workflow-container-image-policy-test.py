#!/usr/bin/env python3
"""Keep every Forgejo job container inside the runner-GC protected mirror.

The shared runner fleet pins `registry.triform.cloud/mirror/*` while pruning
unused Docker layers. A direct Docker Hub tag can therefore disappear after
act_runner resolves it but before `docker create`, producing a false code red.
This repository-local ratchet prevents that cross-repository drift.
"""

from __future__ import annotations

import pathlib
import re


ROOT = pathlib.Path(__file__).resolve().parents[2]
WORKFLOWS = ROOT / ".forgejo" / "workflows"
PROTECTED_PREFIX = "registry.triform.cloud/mirror/"
IMAGE_LINE = re.compile(r"^\s+image:\s*['\"]?([^'\"\s#]+)")


def main() -> int:
    images: list[tuple[pathlib.Path, int, str]] = []
    for workflow in sorted(WORKFLOWS.glob("*.yml")):
        for number, line in enumerate(workflow.read_text().splitlines(), 1):
            match = IMAGE_LINE.match(line)
            if match:
                images.append((workflow, number, match.group(1)))

    if not images:
        raise SystemExit("no workflow container images found; policy would pass vacuously")

    offenders = [row for row in images if not row[2].startswith(PROTECTED_PREFIX)]
    if offenders:
        details = "\n".join(
            f"  {path.relative_to(ROOT)}:{number}: {image}"
            for path, number, image in offenders
        )
        raise SystemExit(
            "workflow container images bypass the runner-GC protected mirror:\n"
            f"{details}\n"
            f"use {PROTECTED_PREFIX}<image>:<tag>"
        )

    print(f"workflow image policy: {len(images)} container image(s) use protected mirror")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
