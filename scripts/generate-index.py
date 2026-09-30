#!/usr/bin/env python3
import argparse
import hashlib
import json
import re
from pathlib import Path
from zipfile import ZipFile


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--artifacts", type=Path, required=True)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--existing", type=Path)
    parser.add_argument("--remove-plugin-ids-json", default="[]")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    try:
        removed_ids = json.loads(args.remove_plugin_ids_json)
    except json.JSONDecodeError as error:
        raise SystemExit(f"invalid removed plugin ID list: {error}") from error
    if not isinstance(removed_ids, list) or any(not isinstance(plugin_id, str) for plugin_id in removed_ids):
        raise SystemExit("removed plugin IDs must be a JSON array of strings")

    if args.existing and args.existing.exists():
        result = json.loads(args.existing.read_text())
        grouped = {
            plugin["id"]: plugin
            for plugin in result.get("plugins", [])
            if isinstance(plugin, dict) and isinstance(plugin.get("id"), str)
        }
    else:
        result = {
            "formatVersion": 1,
            "name": "Lux Plugins",
            "description": "Lux 官方插件目录",
        }
        grouped = {}
    for plugin_id in removed_ids:
        grouped.pop(plugin_id, None)

    for package in sorted(args.artifacts.rglob("*.zip")):
        with ZipFile(package) as archive:
            manifest = json.loads(archive.read("manifest.json"))
        match = re.search(r"-linux-(x86_64|aarch64)\.zip$", package.name)
        if not match:
            raise SystemExit(f"package name does not contain a supported architecture: {package.name}")
        arch = match.group(1)
        entry = {
            "id": manifest["id"],
            "name": manifest["name"],
            "description": manifest.get("description", ""),
            "category": manifest["category"],
            "version": manifest["version"],
            "runtime": manifest["runtime"]["kind"],
            "providerKey": manifest.get("providerKey"),
            "aliases": manifest.get("aliases", []),
            "capabilities": manifest.get("capabilities", []),
            "packages": [],
        }
        if manifest["id"] in grouped and grouped[manifest["id"]]["version"] == manifest["version"]:
            existing_packages = grouped[manifest["id"]].get("packages", [])
            entry["packages"] = list(existing_packages)
        elif manifest["id"] in grouped and grouped[manifest["id"]]["version"] != manifest["version"]:
            entry["packages"] = []
        digest = hashlib.sha256(package.read_bytes()).hexdigest()
        asset = package.name
        package_entry = {
            "platform": "linux",
            "arch": arch,
            "url": f"https://github.com/{args.repository}/releases/download/{manifest['id']}/{asset}",
            "sha256": digest,
        }
        entry["packages"] = [
            existing
            for existing in entry["packages"]
            if not (existing.get("platform") == "linux" and existing.get("arch") == arch)
        ]
        entry["packages"].append(package_entry)
        grouped[manifest["id"]] = entry

    result["plugins"] = sorted(grouped.values(), key=lambda item: item["id"])
    args.output.write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n")


if __name__ == "__main__":
    main()
