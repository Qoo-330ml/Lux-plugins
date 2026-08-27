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
PUBLISH_SCRIPT = ROOT / "scripts" / "publish-releases.py"
INDEX_SCRIPT = ROOT / "scripts" / "generate-index.py"


class ReleaseWorkflowTests(unittest.TestCase):
    def test_tmdb_manifest_exposes_selectable_language_and_api_options(self):
        manifest = json.loads((ROOT / "manifests/org.lux.tmdb.json").read_text())
        fields = {field["key"]: field for field in manifest["configFields"]}

        preferred_language = fields["preferredLanguage"]
        self.assertEqual(preferred_language["type"], "select")
        self.assertEqual(preferred_language["options"][0], {"value": "zh-CN", "label": "简体中文"})
        self.assertEqual(
            [option["value"] for option in preferred_language["options"][:4]],
            ["zh-CN", "zh-SG", "zh-HK", "zh-TW"],
        )
        self.assertGreater(len(preferred_language["options"]), 4)

        fallback_languages = fields["fallbackLanguages"]
        self.assertEqual(fallback_languages["type"], "select")
        self.assertTrue(fallback_languages["multiple"])
        self.assertEqual(
            [option["value"] for option in fallback_languages["options"][:4]],
            ["zh-CN", "zh-SG", "zh-HK", "zh-TW"],
        )

        api_base_url_preset = fields["apiBaseUrlPreset"]
        self.assertEqual(api_base_url_preset["type"], "select")
        self.assertEqual(
            [option["value"] for option in api_base_url_preset["options"]],
            ["official", "alternate", "custom"],
        )

        api_base_url = fields["apiBaseUrl"]
        self.assertEqual(api_base_url["type"], "text")
        self.assertEqual(api_base_url["defaultValue"], "https://api.themoviedb.org")
        self.assertFalse(api_base_url["required"])

    def test_publishes_each_plugin_to_its_own_release_and_reuses_existing_release(self):
        with tempfile.TemporaryDirectory() as temporary_directory:
            temporary = Path(temporary_directory)
            artifacts = temporary / "artifacts"
            artifacts.mkdir()
            write_package(
                artifacts / "x86_64" / "org.lux.alpha-2.0.0-linux-x86_64.zip",
                "org.lux.alpha",
                "2.0.0",
            )
            write_package(
                artifacts / "aarch64" / "org.lux.alpha-2.0.0-linux-aarch64.zip",
                "org.lux.alpha",
                "2.0.0",
            )
            write_package(
                artifacts / "x86_64" / "org.lux.beta-1.0.0-linux-x86_64.zip",
                "org.lux.beta",
                "1.0.0",
            )
            write_package(
                artifacts / "aarch64" / "org.lux.beta-1.0.0-linux-aarch64.zip",
                "org.lux.beta",
                "1.0.0",
            )

            log_path = temporary / "gh.jsonl"
            state_path = temporary / "existing-releases.json"
            state_path.write_text(json.dumps(["org.lux.alpha"]))
            fake_gh = write_fake_gh(temporary / "gh", log_path, state_path)

            environment = os.environ.copy()
            environment["PATH"] = f"{fake_gh.parent}{os.pathsep}{environment['PATH']}"
            subprocess.run(
                [
                    sys.executable,
                    str(PUBLISH_SCRIPT),
                    "--artifacts",
                    str(artifacts),
                    "--repository",
                    "Qoo-330ml/Lux-plugins",
                    "--target",
                    "commit-sha",
                ],
                cwd=ROOT,
                env=environment,
                check=True,
            )

            commands = [json.loads(line) for line in log_path.read_text().splitlines()]
            self.assertEqual(commands[0], ["release", "view", "--repo", "Qoo-330ml/Lux-plugins", "org.lux.alpha"])
            self.assertEqual(commands[1][0:4], ["release", "upload", "--repo", "Qoo-330ml/Lux-plugins"])
            self.assertEqual(commands[1][4], "org.lux.alpha")
            self.assertIn("--clobber", commands[1])
            self.assertIn("org.lux.alpha-2.0.0-linux-x86_64.zip", package_names(commands[1]))
            self.assertIn("org.lux.alpha-2.0.0-linux-aarch64.zip", package_names(commands[1]))

            create_command = commands[3]
            self.assertEqual(create_command[0:4], ["release", "create", "--repo", "Qoo-330ml/Lux-plugins"])
            self.assertEqual(create_command[4], "org.lux.beta")
            self.assertIn("--target", create_command)
            self.assertIn("commit-sha", create_command)
            self.assertIn("org.lux.beta-1.0.0-linux-x86_64.zip", package_names(create_command))
            self.assertIn("org.lux.beta-1.0.0-linux-aarch64.zip", package_names(create_command))

    def test_catalog_uses_the_plugin_release_tag_for_each_package(self):
        with tempfile.TemporaryDirectory() as temporary_directory:
            temporary = Path(temporary_directory)
            artifacts = temporary / "artifacts"
            package = artifacts / "x86_64" / "org.lux.alpha-2.0.0-linux-x86_64.zip"
            write_package(package, "org.lux.alpha", "2.0.0")
            output = temporary / "index.json"

            subprocess.run(
                [
                    sys.executable,
                    str(INDEX_SCRIPT),
                    "--artifacts",
                    str(artifacts),
                    "--repository",
                    "Qoo-330ml/Lux-plugins",
                    "--output",
                    str(output),
                ],
                cwd=ROOT,
                check=True,
            )

            catalog = json.loads(output.read_text())
            package_url = catalog["plugins"][0]["packages"][0]["url"]
            self.assertEqual(
                package_url,
                "https://github.com/Qoo-330ml/Lux-plugins/releases/download/"
                "org.lux.alpha/org.lux.alpha-2.0.0-linux-x86_64.zip",
            )

    def test_publish_script_rejects_empty_artifact_directory(self):
        with tempfile.TemporaryDirectory() as temporary_directory:
            result = subprocess.run(
                [
                    sys.executable,
                    str(PUBLISH_SCRIPT),
                    "--artifacts",
                    temporary_directory,
                    "--repository",
                    "Qoo-330ml/Lux-plugins",
                    "--target",
                    "commit-sha",
                ],
                cwd=ROOT,
                capture_output=True,
                text=True,
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertIn("no plugin packages found", result.stderr)


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


def package_names(command: list[str]) -> list[str]:
    return [Path(argument).name for argument in command if argument.endswith(".zip")]


def write_fake_gh(path: Path, log_path: Path, state_path: Path) -> Path:
    path.write_text(
        """#!/usr/bin/env python3
import json
import sys
from pathlib import Path

args = sys.argv[1:]
log_path = Path(%r)
state_path = Path(%r)
with log_path.open("a") as log:
    log.write(json.dumps(args) + "\\n")

command = args[0:2]
tag = args[4] if len(args) > 4 and args[2:4] == ["--repo", "Qoo-330ml/Lux-plugins"] else ""
state = json.loads(state_path.read_text())
if command == ["release", "view"]:
    raise SystemExit(0 if tag in state else 1)
if command == ["release", "create"]:
    state.append(tag)
    state_path.write_text(json.dumps(state))
""" % (str(log_path), str(state_path))
    )
    path.chmod(path.stat().st_mode | stat.S_IXUSR)
    return path


if __name__ == "__main__":
    unittest.main()
