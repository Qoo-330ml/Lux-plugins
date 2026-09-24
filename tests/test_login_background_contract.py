import json
import unittest
from pathlib import Path
from urllib.parse import urlsplit


ROOT = Path(__file__).resolve().parents[1]
FIXTURES = ROOT / "tests/fixtures/login-background"
IMAGE_TYPES = {"SINGLE_POSTER", "SINGLE_IMAGE", "HERO_IMAGE"}


class LoginBackgroundContractTests(unittest.TestCase):
    def test_independent_commons_and_tmdb_providers_are_registered_for_release(self):
        plugins = json.loads((ROOT / "plugins.json").read_text())
        plugins_by_id = {plugin["id"]: plugin for plugin in plugins}

        expected = {
            "org.lux.wikimedia-potd-background": {
                "id": "org.lux.wikimedia-potd-background",
                "binary": "lux-plugin-wikimedia-potd-background",
                "version": "0.1.0",
                "manifest": "manifests/org.lux.wikimedia-potd-background.json",
            },
            "org.lux.tmdb-trending-background": {
                "id": "org.lux.tmdb-trending-background",
                "binary": "lux-plugin-tmdb-trending-background",
                "version": "0.1.0",
                "manifest": "manifests/org.lux.tmdb-trending-background.json",
            },
        }
        for plugin_id, entry in expected.items():
            with self.subTest(plugin=plugin_id):
                self.assertEqual(plugins_by_id[plugin_id], entry)
                manifest = json.loads((ROOT / entry["manifest"]).read_text())
                self.assertEqual(manifest["id"], plugin_id)
                self.assertEqual(manifest["type"], "login_background")
                self.assertEqual(manifest["capabilities"], ["login_background.get"])

        tmdb_manifest = json.loads(
            (ROOT / expected["org.lux.tmdb-trending-background"]["manifest"]).read_text()
        )
        self.assertEqual(
            [field["key"] for field in tmdb_manifest["configFields"]],
            ["licenseReviewed"],
        )

    def test_provider_fixtures_match_manifest_hosts_and_bounded_response_contract(self):
        manifest = json.loads((FIXTURES / "manifest-v1.json").read_text())
        image_hosts = {host.lower() for host in manifest["permissions"]["imageHosts"]}
        network_hosts = {host.lower() for host in manifest["permissions"].get("network", [])}
        fixture_names = (
            "poster-feed-v1.json",
            "hero-image-v1.json",
            "single-poster-v1.json",
            "single-image-v1.json",
        )

        for fixture_name in fixture_names:
            with self.subTest(fixture=fixture_name):
                payload = json.loads((FIXTURES / fixture_name).read_text())
                self.assertLessEqual(
                    set(payload),
                    {"contentKind", "sourceName", "copyrightNotice", "items"},
                )
                self.assertIn(
                    payload["contentKind"],
                    {"POSTER_FEED", "HERO_IMAGE", "SINGLE_POSTER", "SINGLE_IMAGE"},
                )
                self.assertTrue(payload["sourceName"].strip())
                self.assertLessEqual(len(payload["items"]), 40)
                if payload["contentKind"] in IMAGE_TYPES:
                    self.assertEqual(len(payload["items"]), 1)
                self.assertLessEqual(len(json.dumps(payload).encode()), 256 * 1024)

                for item in payload["items"]:
                    self.assertLessEqual(
                        set(item),
                        {
                            "imageUrl",
                            "title",
                            "copyrightNotice",
                            "attributionUrl",
                            "licenseUrl",
                        },
                    )
                    self.assert_declared_https_url(item["imageUrl"], image_hosts)
                    for key in ("attributionUrl", "licenseUrl"):
                        if key in item:
                            self.assert_declared_https_url(item[key], network_hosts)

    def assert_declared_https_url(self, value, declared_hosts):
        parsed = urlsplit(value)
        self.assertEqual(parsed.scheme, "https")
        self.assertIn(parsed.hostname.lower(), declared_hosts)
        self.assertIsNone(parsed.username)
        self.assertIsNone(parsed.password)
        self.assertIsNone(parsed.port)
        self.assertFalse(parsed.fragment)


if __name__ == "__main__":
    unittest.main()
