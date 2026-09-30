import hashlib
import json
import os
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from zipfile import ZIP_DEFLATED, ZipFile


ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))

from release_plan import plan_release, select_plugins


class ReleasePipelinePolicyTests(unittest.TestCase):
    def test_release_plan_selects_only_added_or_version_bumped_plugins(self):
        previous = [
            {"id": "org.example.same", "version": "1.0.0", "name": "Old name"},
            {"id": "org.example.bump", "version": "1.0.0"},
            {"id": "org.example.removed", "version": "1.0.0"},
        ]
        current = [
            {"id": "org.example.same", "version": "1.0.0", "name": "New name"},
            {"id": "org.example.bump", "version": "1.1.0"},
            {"id": "org.example.added", "version": "0.1.0"},
        ]

        plan = plan_release(previous, current)

        self.assertEqual(plan["releasePluginIds"], ["org.example.added", "org.example.bump"])
        self.assertEqual(plan["removedPluginIds"], ["org.example.removed"])
        self.assertTrue(plan["hasWork"])

    def test_source_or_manifest_change_without_a_version_bump_does_not_publish(self):
        previous = [{"id": "org.example.plugin", "version": "1.0.0"}]
        current = [{"id": "org.example.plugin", "version": "1.0.0", "description": "Changed"}]

        plan = plan_release(previous, current)

        self.assertEqual(plan["releasePluginIds"], [])
        self.assertEqual(plan["removedPluginIds"], [])
        self.assertFalse(plan["hasWork"])

    def test_release_plan_cli_compares_the_base_revision_with_current_catalog(self):
        with tempfile.TemporaryDirectory() as temporary_directory:
            temporary = Path(temporary_directory)
            subprocess.run(["git", "init", "-q"], cwd=temporary, check=True)
            subprocess.run(["git", "config", "user.name", "Release test"], cwd=temporary, check=True)
            subprocess.run(["git", "config", "user.email", "release-test@example.invalid"], cwd=temporary, check=True)
            catalog = temporary / "plugins.json"
            catalog.write_text(json.dumps([{"id": "org.example.plugin", "version": "1.0.0"}]))
            subprocess.run(["git", "add", "plugins.json"], cwd=temporary, check=True)
            subprocess.run(["git", "commit", "-qm", "base catalog"], cwd=temporary, check=True)
            catalog.write_text(json.dumps([
                {"id": "org.example.plugin", "version": "1.1.0"},
                {"id": "org.example.new", "version": "0.1.0"},
            ]))
            subprocess.run(["git", "add", "plugins.json"], cwd=temporary, check=True)
            subprocess.run(["git", "commit", "-qm", "bump plugin versions"], cwd=temporary, check=True)
            result = subprocess.run([
                sys.executable,
                str(ROOT / "scripts/release_plan.py"),
                "--base-ref", "HEAD^",
                "--current", str(catalog),
            ], cwd=temporary, check=True, capture_output=True, text=True)

            self.assertEqual(json.loads(result.stdout), {
                "releasePluginIds": ["org.example.new", "org.example.plugin"],
                "removedPluginIds": [],
                "hasWork": True,
            })

    def test_release_plan_cli_rejects_option_like_base_refs(self):
        result = subprocess.run([
            sys.executable,
            str(ROOT / "scripts/release_plan.py"),
            "--base-ref=--output=/tmp/untrusted",
        ], cwd=ROOT, capture_output=True, text=True)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unsupported characters", result.stderr)

    def test_package_selection_preserves_catalog_order_and_rejects_unknown_ids(self):
        catalog = [
            {"id": "org.example.one", "version": "1.0.0"},
            {"id": "org.example.two", "version": "1.0.0"},
            {"id": "org.example.three", "version": "1.0.0"},
        ]

        self.assertEqual(
            select_plugins(catalog, ["org.example.three", "org.example.one"]),
            [catalog[0], catalog[2]],
        )
        with self.assertRaisesRegex(ValueError, "unknown plugin IDs"):
            select_plugins(catalog, ["org.example.missing"])

    def test_index_merge_preserves_untouched_plugins_and_removes_only_requested_ids(self):
        with tempfile.TemporaryDirectory() as temporary_directory:
            temporary = Path(temporary_directory)
            artifacts = temporary / "artifacts"
            package = artifacts / "x86_64" / "org.example.alpha-2.0.0-linux-x86_64.zip"
            write_package(package, "org.example.alpha", "2.0.0")
            existing = temporary / "existing-index.json"
            existing.write_text(json.dumps({
                "formatVersion": 1,
                "name": "Lux Plugins",
                "description": "Existing description",
                "plugins": [
                    {"id": "org.example.alpha", "version": "1.0.0", "packages": []},
                    {"id": "org.example.beta", "version": "4.0.0", "packages": [{"url": "keep"}]},
                    {"id": "org.example.removed", "version": "1.0.0", "packages": []},
                ],
            }))
            output = temporary / "index.json"

            subprocess.run([
                sys.executable,
                str(ROOT / "scripts/generate-index.py"),
                "--artifacts", str(artifacts),
                "--repository", "Qoo-330ml/Lux-plugins",
                "--existing", str(existing),
                "--remove-plugin-ids-json", '["org.example.removed"]',
                "--output", str(output),
            ], check=True)

            index = json.loads(output.read_text())
            plugins = {plugin["id"]: plugin for plugin in index["plugins"]}
            self.assertEqual(index["description"], "Existing description")
            self.assertEqual(set(plugins), {"org.example.alpha", "org.example.beta"})
            self.assertEqual(plugins["org.example.alpha"]["version"], "2.0.0")
            self.assertEqual(plugins["org.example.beta"]["packages"], [{"url": "keep"}])

    def test_index_can_apply_catalog_removals_without_building_packages(self):
        with tempfile.TemporaryDirectory() as temporary_directory:
            temporary = Path(temporary_directory)
            artifacts = temporary / "empty-artifacts"
            artifacts.mkdir()
            existing = temporary / "existing-index.json"
            existing.write_text(json.dumps({"plugins": [
                {"id": "org.example.keep", "version": "1.0.0", "packages": []},
                {"id": "org.example.remove", "version": "1.0.0", "packages": []},
            ]}))
            output = temporary / "index.json"

            subprocess.run([
                sys.executable,
                str(ROOT / "scripts/generate-index.py"),
                "--artifacts", str(artifacts),
                "--repository", "Qoo-330ml/Lux-plugins",
                "--existing", str(existing),
                "--remove-plugin-ids-json", '["org.example.remove"]',
                "--output", str(output),
            ], check=True)

            catalog = json.loads(output.read_text())
            self.assertEqual([plugin["id"] for plugin in catalog["plugins"]], ["org.example.keep"])

    def test_package_plugin_writes_reproducible_archives(self):
        with tempfile.TemporaryDirectory() as temporary_directory:
            temporary = Path(temporary_directory)
            manifest = temporary / "manifest.json"
            manifest.write_text(json.dumps({
                "formatVersion": 1,
                "id": "org.example.plugin",
                "name": "Example",
                "version": "__VERSION__",
                "apiVersion": 1,
                "runtime": {"kind": "process", "entrypoint": "__ENTRYPOINT__"},
                "type": "utility",
                "category": "UTILITY",
                "capabilities": [],
                "files": [],
            }))
            binary = temporary / "lux-plugin-example"
            binary.write_bytes(b"stable binary payload")
            outputs = [temporary / "first.zip", temporary / "second.zip"]

            for output in outputs:
                subprocess.run([
                    sys.executable,
                    str(ROOT / "scripts/package_plugin.py"),
                    "--id", "org.example.plugin",
                    "--version", "1.0.0",
                    "--manifest", str(manifest),
                    "--binary", str(binary),
                    "--platform", "linux",
                    "--arch", "x86_64",
                    "--output", str(output),
                ], check=True)

            self.assertEqual(outputs[0].read_bytes(), outputs[1].read_bytes())

    def test_publisher_never_clobbers_and_skips_assets_already_in_the_catalog(self):
        with tempfile.TemporaryDirectory() as temporary_directory:
            temporary = Path(temporary_directory)
            artifacts = temporary / "artifacts"
            package = artifacts / "x86_64" / "org.example.alpha-2.0.0-linux-x86_64.zip"
            write_package(package, "org.example.alpha", "2.0.0")
            index = temporary / "index.json"
            index.write_text(json.dumps({"plugins": [{
                "id": "org.example.alpha",
                "packages": [{"url": "https://github.com/Qoo-330ml/Lux-plugins/releases/download/org.example.alpha/org.example.alpha-1.0.0-linux-x86_64.zip", "sha256": "old"}],
            }]}))
            log = temporary / "gh.jsonl"
            fake_gh = write_fake_gh(temporary / "gh", log, ["org.example.alpha"])
            environment = os.environ.copy()
            environment["PATH"] = f"{fake_gh.parent}{os.pathsep}{environment['PATH']}"
            command = [
                sys.executable,
                str(ROOT / "scripts/publish-releases.py"),
                "--artifacts", str(artifacts),
                "--repository", "Qoo-330ml/Lux-plugins",
                "--target", "commit-sha",
                "--existing-index", str(index),
            ]

            subprocess.run(command, cwd=ROOT, env=environment, check=True)
            commands = [json.loads(line) for line in log.read_text().splitlines()]
            upload = next(command for command in commands if command[:2] == ["release", "upload"])
            self.assertNotIn("--clobber", upload)
            self.assertTrue(any(str(package) == argument for argument in upload))

            package_hash = hashlib.sha256(package.read_bytes()).hexdigest()
            index.write_text(json.dumps({"plugins": [{
                "id": "org.example.alpha",
                "packages": [{"url": f"https://github.com/Qoo-330ml/Lux-plugins/releases/download/org.example.alpha/{package.name}", "sha256": package_hash}],
            }]}))
            log.write_text("")
            subprocess.run(command, cwd=ROOT, env=environment, check=True)
            commands = [json.loads(line) for line in log.read_text().splitlines()]
            self.assertEqual(commands, [["release", "view", "--repo", "Qoo-330ml/Lux-plugins", "org.example.alpha"]])

    def test_publisher_rejects_different_content_for_an_already_cataloged_asset(self):
        with tempfile.TemporaryDirectory() as temporary_directory:
            temporary = Path(temporary_directory)
            artifacts = temporary / "artifacts"
            package = artifacts / "x86_64" / "org.example.alpha-2.0.0-linux-x86_64.zip"
            write_package(package, "org.example.alpha", "2.0.0")
            index = temporary / "index.json"
            index.write_text(json.dumps({"plugins": [{
                "id": "org.example.alpha",
                "packages": [{"url": f"https://github.com/Qoo-330ml/Lux-plugins/releases/download/org.example.alpha/{package.name}", "sha256": "different"}],
            }]}))
            log = temporary / "gh.jsonl"
            fake_gh = write_fake_gh(temporary / "gh", log, ["org.example.alpha"])
            environment = os.environ.copy()
            environment["PATH"] = f"{fake_gh.parent}{os.pathsep}{environment['PATH']}"

            result = subprocess.run([
                sys.executable,
                str(ROOT / "scripts/publish-releases.py"),
                "--artifacts", str(artifacts),
                "--repository", "Qoo-330ml/Lux-plugins",
                "--target", "commit-sha",
                "--existing-index", str(index),
            ], cwd=ROOT, env=environment, capture_output=True, text=True)

            self.assertNotEqual(result.returncode, 0)
            self.assertIn("bump the plugin version", result.stderr)
            self.assertFalse(log.exists())

    def test_publisher_does_not_recreate_a_missing_release_already_in_the_catalog(self):
        with tempfile.TemporaryDirectory() as temporary_directory:
            temporary = Path(temporary_directory)
            artifacts = temporary / "artifacts"
            x86_package = artifacts / "x86_64" / "org.example.alpha-2.0.0-linux-x86_64.zip"
            arm_package = artifacts / "aarch64" / "org.example.alpha-2.0.0-linux-aarch64.zip"
            write_package(x86_package, "org.example.alpha", "2.0.0")
            write_package(arm_package, "org.example.alpha", "2.0.0")
            index = temporary / "index.json"
            index.write_text(json.dumps({"plugins": [{
                "id": "org.example.alpha",
                "packages": [{
                    "url": f"https://github.com/Qoo-330ml/Lux-plugins/releases/download/org.example.alpha/{x86_package.name}",
                    "sha256": hashlib.sha256(x86_package.read_bytes()).hexdigest(),
                }],
            }]}))
            log = temporary / "gh.jsonl"
            fake_gh = write_fake_gh(temporary / "gh", log, [])
            environment = os.environ.copy()
            environment["PATH"] = f"{fake_gh.parent}{os.pathsep}{environment['PATH']}"

            result = subprocess.run([
                sys.executable,
                str(ROOT / "scripts/publish-releases.py"),
                "--artifacts", str(artifacts),
                "--repository", "Qoo-330ml/Lux-plugins",
                "--target", "commit-sha",
                "--existing-index", str(index),
            ], cwd=ROOT, env=environment, capture_output=True, text=True)

            self.assertNotEqual(result.returncode, 0)
            self.assertIn("GitHub release is missing", result.stderr)
            self.assertEqual(
                [json.loads(line) for line in log.read_text().splitlines()],
                [["release", "view", "--repo", "Qoo-330ml/Lux-plugins", "org.example.alpha"]],
            )


def write_package(path: Path, plugin_id: str, version: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    manifest = {
        "id": plugin_id,
        "name": plugin_id,
        "description": "test plugin",
        "version": version,
        "category": "TEST",
        "runtime": {"kind": "process"},
        "capabilities": [],
    }
    with ZipFile(path, "w", compression=ZIP_DEFLATED) as archive:
        archive.writestr("manifest.json", json.dumps(manifest))


def write_fake_gh(path: Path, log_path: Path, existing_releases: list[str]) -> Path:
    path.write_text(
        """#!/usr/bin/env python3
import json
import sys
from pathlib import Path

args = sys.argv[1:]
log_path = Path(%r)
with log_path.open("a") as log:
    log.write(json.dumps(args) + "\\n")
tag = args[4] if len(args) > 4 and args[2:4] == ["--repo", "Qoo-330ml/Lux-plugins"] else ""
if args[0:2] == ["release", "view"]:
    raise SystemExit(0 if tag in %r else 1)
""" % (str(log_path), existing_releases)
    )
    path.chmod(path.stat().st_mode | stat.S_IXUSR)
    return path


if __name__ == "__main__":
    unittest.main()
