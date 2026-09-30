#!/usr/bin/env python3
import argparse
import hashlib
import json
import subprocess
from collections import defaultdict
from pathlib import Path
from typing import Optional
from zipfile import ZipFile


def main() -> None:
    parser = argparse.ArgumentParser(description="Publish each plugin to its stable GitHub release.")
    parser.add_argument("--artifacts", type=Path, required=True)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--target", required=True)
    parser.add_argument("--existing-index", type=Path)
    args = parser.parse_args()

    catalog_hashes = load_catalog_hashes(args.existing_index)

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
        to_upload = []
        has_cataloged_asset = False
        for package in packages:
            asset_url = (
                f"https://github.com/{args.repository}/releases/download/{plugin_id}/{package.name}"
            )
            package_hash = hashlib.sha256(package.read_bytes()).hexdigest()
            existing_hash = catalog_hashes.get(asset_url)
            if existing_hash is not None:
                has_cataloged_asset = True
                if existing_hash != package_hash:
                    raise SystemExit(
                        f"refusing to replace published asset {package.name}; "
                        "bump the plugin version before publishing changed content"
                    )
                continue
            to_upload.append(package)

        release_exists = subprocess.run(
            ["gh", "release", "view", "--repo", args.repository, plugin_id],
            check=False,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        ).returncode == 0
        if release_exists:
            if to_upload:
                run_gh(
                    [
                        "release",
                        "upload",
                        "--repo",
                        args.repository,
                        plugin_id,
                        *(str(package) for package in to_upload),
                    ]
                )
        else:
            if has_cataloged_asset:
                raise SystemExit(
                    f"catalog references packages for {plugin_id}, but its GitHub release is missing"
                )
            run_gh(
                [
                    "release",
                    "create",
                    "--repo",
                    args.repository,
                    plugin_id,
                    *(str(package) for package in to_upload),
                    "--target",
                    args.target,
                    "--title",
                    f"Lux plugin {plugin_id}",
                    "--notes",
                    f"Automated package release for {plugin_id}.",
                ]
            )


def load_catalog_hashes(index_path: Optional[Path]) -> dict[str, str]:
    if index_path is None or not index_path.exists():
        return {}
    try:
        catalog = json.loads(index_path.read_text())
    except (OSError, json.JSONDecodeError) as error:
        raise SystemExit(f"could not read existing plugin index: {error}") from error
    hashes = {}
    for plugin in catalog.get("plugins", []):
        if not isinstance(plugin, dict):
            continue
        for package in plugin.get("packages", []):
            if not isinstance(package, dict):
                continue
            url = package.get("url")
            sha256 = package.get("sha256")
            if isinstance(url, str) and isinstance(sha256, str):
                hashes[url] = sha256
    return hashes


def run_gh(arguments: list[str]) -> None:
    subprocess.run(["gh", *arguments], check=True)


if __name__ == "__main__":
    main()
