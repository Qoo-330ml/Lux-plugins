#!/usr/bin/env python3
import argparse
import json
import subprocess
from collections import defaultdict
from pathlib import Path
from zipfile import ZipFile


def main() -> None:
    parser = argparse.ArgumentParser(description="Publish each plugin to its stable GitHub release.")
    parser.add_argument("--artifacts", type=Path, required=True)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--target", required=True)
    args = parser.parse_args()

    packages_by_plugin = defaultdict(list)
    for package in sorted(args.artifacts.rglob("*.zip")):
        with ZipFile(package) as archive:
            manifest = json.loads(archive.read("manifest.json"))
        plugin_id = manifest.get("id")
        if not plugin_id:
            raise SystemExit(f"package manifest has no plugin id: {package}")
        packages_by_plugin[plugin_id].append(package)

    if not packages_by_plugin:
        raise SystemExit(f"no plugin packages found under {args.artifacts}")

    for plugin_id, packages in sorted(packages_by_plugin.items()):
        release_exists = subprocess.run(
            ["gh", "release", "view", "--repo", args.repository, plugin_id],
            check=False,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        ).returncode == 0
        if release_exists:
            run_gh(
                [
                    "release",
                    "upload",
                    "--repo",
                    args.repository,
                    plugin_id,
                    "--clobber",
                    *(str(package) for package in packages),
                ]
            )
        else:
            run_gh(
                [
                    "release",
                    "create",
                    "--repo",
                    args.repository,
                    plugin_id,
                    *(str(package) for package in packages),
                    "--target",
                    args.target,
                    "--title",
                    f"Lux plugin {plugin_id}",
                    "--notes",
                    f"Automated package release for {plugin_id}.",
                ]
            )


def run_gh(arguments: list[str]) -> None:
    subprocess.run(["gh", *arguments], check=True)


if __name__ == "__main__":
    main()
