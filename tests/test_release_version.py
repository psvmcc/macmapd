"""Release publication must use the version compiled into the executable."""

import importlib.util
import unittest
from pathlib import Path

spec = importlib.util.spec_from_file_location(
    "release_version", Path(__file__).resolve().parent.parent / "scripts/check-release-version.py"
)
release_version = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release_version)


class ReleaseVersionTests(unittest.TestCase):
    def test_matching_versions(self):
        release_version.validate("v1.2.3", {"package": {"name": "macdack", "version": "1.2.3"}},
                                 {"package": [{"name": "macdack", "version": "1.2.3"}]})

    def test_rejects_invalid_and_mismatched_versions(self):
        manifest = {"package": {"name": "macdack", "version": "1.2.3"}}
        lockfile = {"package": [{"name": "macdack", "version": "1.2.3"}]}
        for tag in ["1.2.3", "v1.2.4", "v1.2.3-rc.1", "v1.2.3+build", "v01.2.3"]:
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                release_version.validate(tag, manifest, lockfile)
        with self.assertRaises(ValueError):
            release_version.validate("v1.2.3", manifest,
                                     {"package": [{"name": "macdack", "version": "1.2.2"}]})
