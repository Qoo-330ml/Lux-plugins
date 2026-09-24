import json
import unittest
from pathlib import Path
from urllib.parse import urlsplit


ROOT = Path(__file__).resolve().parents[1]
FIXTURES = ROOT / "tests/fixtures/login-background"
IMAGE_TYPES = {"SINGLE_POSTER", "SINGLE_IMAGE", "HERO_IMAGE"}


class LoginBackgroundContractTests(unittest.TestCase):
    def test_only_the_license_filtered_commons_provider_is_registered_for_release(self):
        plugins = json.loads((ROOT / "plugins.json").read_text())
        plugins_by_id = {plugin["id"]: plugin for plugin in plugins}

        self.assertEqual(
            plugins_by_id["org.lux.wikimedia-potd-background"],
            {
                "id": "org.lux.wikimedia-potd-background",
                "binary": "lux-plugin-wikimedia-potd-background",
                "version": "0.1.0",
                "manifest": "manifests/org.lux.wikimedia-potd-background.json",
            },
        )
        self.assertNotIn("org.lux.tmdb-trending-background", plugins_by_id)

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
