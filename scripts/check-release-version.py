"""Reject release tags that disagree with the application or lockfile version."""

import re
import sys
import tomllib
from pathlib import Path


def validate(tag, manifest, lockfile):
    version = manifest["package"]["version"]
    name = manifest["package"]["name"]
    if not re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", tag):
        raise ValueError("stable releases require an X.Y.Z tag")
    if tag != version:
        raise ValueError(f"tag {tag} does not match Cargo.toml version {version}")
    packages = [p for p in lockfile["package"] if p["name"] == name and "source" not in p]
    if len(packages) != 1 or packages[0]["version"] != version:
        raise ValueError("Cargo.lock application version does not match Cargo.toml")


if __name__ == "__main__":
    root = Path(__file__).resolve().parent.parent
    try:
        if len(sys.argv) != 2:
            raise ValueError("usage: check-release-version.py X.Y.Z")
        validate(
            sys.argv[1],
            tomllib.loads((root / "Cargo.toml").read_text()),
            tomllib.loads((root / "Cargo.lock").read_text()),
        )
    except (ValueError, KeyError) as error:
        sys.exit(str(error))
    print(f"Release version verified: {sys.argv[1]}")
