#!/usr/bin/env python3
import argparse
import json
import re
import subprocess
from pathlib import Path
from typing import Any, Optional


def _catalog_by_id(catalog: Any, label: str) -> dict[str, dict[str, Any]]:
    if not isinstance(catalog, list):
        raise ValueError(f"{label} plugin catalog must be a JSON array")

    plugins: dict[str, dict[str, Any]] = {}
    for plugin in catalog:
        if not isinstance(plugin, dict):
            raise ValueError(f"{label} plugin catalog entries must be objects")
        plugin_id = plugin.get("id")
        version = plugin.get("version")
        if not isinstance(plugin_id, str) or not plugin_id:
            raise ValueError(f"{label} plugin catalog entry has no valid id")
        if not isinstance(version, str) or not version:
            raise ValueError(f"{label} plugin {plugin_id} has no valid version")
        if plugin_id in plugins:
            raise ValueError(f"{label} plugin catalog contains duplicate id {plugin_id}")
        plugins[plugin_id] = plugin
    return plugins


def plan_release(previous_catalog: Any, current_catalog: Any) -> dict[str, Any]:
    """Select only new/version-changed plugins and separately report removed IDs."""
    previous = _catalog_by_id(previous_catalog, "previous")
    current = _catalog_by_id(current_catalog, "current")
    release_plugin_ids = sorted(
        plugin_id
        for plugin_id, plugin in current.items()
        if plugin_id not in previous or plugin["version"] != previous[plugin_id]["version"]
    )
    removed_plugin_ids = sorted(previous.keys() - current.keys())
    return {
        "releasePluginIds": release_plugin_ids,
        "removedPluginIds": removed_plugin_ids,
        "hasWork": bool(release_plugin_ids or removed_plugin_ids),
    }


def select_plugins(catalog: list[dict[str, Any]], plugin_ids: Optional[list[str]]) -> list[dict[str, Any]]:
    if plugin_ids is None:
        return catalog
    if not isinstance(plugin_ids, list) or any(not isinstance(plugin_id, str) for plugin_id in plugin_ids):
        raise ValueError("plugin IDs must be a JSON array of strings")
    if len(plugin_ids) != len(set(plugin_ids)):
        raise ValueError("plugin IDs must not contain duplicates")
    by_id = _catalog_by_id(catalog, "current")
    unknown = sorted(set(plugin_ids) - by_id.keys())
    if unknown:
        raise ValueError(f"unknown plugin IDs in release selection: {', '.join(unknown)}")
    selected = set(plugin_ids)
    return [plugin for plugin in catalog if plugin.get("id") in selected]


def main() -> None:
    parser = argparse.ArgumentParser(description="Select plugin releases from catalog version changes.")
    parser.add_argument("--base-ref", required=True, help="Git revision containing the previous plugins.json")
    parser.add_argument("--current", type=Path, default=Path("plugins.json"))
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()

    try:
        if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._/-]{0,254}(?:[~^][0-9]*)*", args.base_ref):
            raise ValueError("base revision contains unsupported characters")
        base_commit = subprocess.run(
            ["git", "rev-parse", "--verify", "--end-of-options", f"{args.base_ref}^{{commit}}"],
            check=True,
            capture_output=True,
            text=True,
        ).stdout.strip()
        previous_text = subprocess.run(
            ["git", "show", f"{base_commit}:plugins.json"],
            check=True,
            capture_output=True,
            text=True,
        ).stdout
        previous = json.loads(previous_text)
        current = json.loads(args.current.read_text())
        plan = plan_release(previous, current)
    except (subprocess.CalledProcessError, OSError, json.JSONDecodeError, ValueError) as error:
        raise SystemExit(f"could not plan plugin releases: {error}") from error

    serialized = json.dumps(plan, separators=(",", ":")) + "\n"
    if args.output:
        args.output.write_text(serialized)
    else:
        print(serialized, end="")


if __name__ == "__main__":
    main()
