"""Configuration boundaries are pure and independently executable."""

import importlib.machinery
import importlib.util
import tempfile
import unittest
from pathlib import Path

path = Path(__file__).resolve().parents[1] / "ferry"
loader = importlib.machinery.SourceFileLoader("ferry", str(path))
spec = importlib.util.spec_from_loader(loader.name, loader)
ferry = importlib.util.module_from_spec(spec)
loader.exec_module(ferry)


class RoutingTest(unittest.TestCase):
    def test_no_machine_config_is_local(self):
        self.assertEqual(("local", {}), ferry.route({"routable": True}, {}))

    def test_features_are_containment_not_partial_overlap(self):
        recipe = {"routable": True, "requires": ["linux", "quiet"]}
        with self.assertRaises(ferry.Refused):
            ferry.route(recipe, {"busy": {"features": ["linux"]}})
        self.assertEqual(
            "bench", ferry.route(recipe, {"bench": {"features": ["linux", "quiet"]}})[0]
        )

    def test_explicit_unknown_machine_does_not_fallback(self):
        with self.assertRaises(ferry.Refused):
            ferry.route({"routable": True}, {}, on="unknown")

    def test_certifying_recipe_does_not_route(self):
        self.assertEqual("local", ferry.route({}, {"bench": {}})[0])
        with self.assertRaises(ferry.Refused):
            ferry.route({}, {"bench": {}}, on="bench")

    def test_catalog_pointer_is_the_only_home(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "gates.toml").write_text('[recipe.test]\nrequires = ["linux"]\n')
            entries = ferry.catalog(
                root, {"catalog": {"path": "gates.toml"}, "recipe": {"wrong": {}}}
            )
            self.assertEqual({"test": {"requires": ["linux"]}}, entries)


if __name__ == "__main__":
    unittest.main()
