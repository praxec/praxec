import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

from prepare import prepare


class PrepareTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.models = self.root / "models.yaml"
        self.models.write_text('version: 1\ndefault:\n  - provider: {name: openrouter}\n    model: mock\n')

    def test_existing_config_is_preserved(self):
        path = prepare(self.root / "run", self.models, "coding")
        original = path.read_bytes()
        with self.assertRaises(FileExistsError):
            prepare(path.parent, self.models, "reasoning")
        self.assertEqual(path.read_bytes(), original)

    def test_invalid_bounds_do_not_create_run(self):
        for wall, budget in [(0, 300), (301, 300), (180, 3601)]:
            with self.assertRaises(ValueError):
                prepare(self.root / "invalid", self.models, "coding", wall, budget)
        self.assertFalse((self.root / "invalid").exists())

    @unittest.skipUnless(shutil.which("praxec"), "requires installed praxec")
    def test_runtime_starts_with_a_separate_worker_and_acceptance_step(self):
        path = prepare(self.root / "run", self.models, "coding")
        subprocess.run(["praxec", "check", "--config", str(path)],
                       check=True, capture_output=True, text=True)
        payload = {"definitionId": "delegated_task", "input": {
            "task": "Review", "context": "Source", "acceptance": "Evidence"}}
        result = subprocess.run(
            ["praxec", "command", "--config", str(path), json.dumps(payload)],
            check=True, capture_output=True, text=True,
        )
        response = json.loads(result.stdout)
        self.assertEqual(response["workflow"]["state"], "ready")
        self.assertEqual([link["rel"] for link in response["links"]], ["run"])
        config = json.loads(path.read_text())
        states = config["workflows"]["delegated_task"]["states"]
        self.assertEqual(states["ready"]["transitions"]["run"]["target"], "awaiting_review")
        self.assertEqual(states["ready"]["transitions"]["run"]["executor"]["tools"], [])
        self.assertEqual(states["rejected"]["outcome"], "failure")

    @unittest.skipUnless(shutil.which("praxec"), "requires installed praxec")
    def test_acceptance_requires_evidence_and_persists_it(self):
        path = prepare(self.root / "gate", self.models, "coding")
        config = json.loads(path.read_text())
        # Substitute only the paid worker; exercise the real runtime, mapping,
        # input validation, persistence, and acceptance transition offline.
        config["workflows"]["delegated_task"]["states"]["ready"]["transitions"]["run"]["executor"] = {"kind": "noop"}
        path.write_text(json.dumps(config))

        def call(payload):
            r = subprocess.run(["praxec", "command", "--config", str(path), json.dumps(payload)],
                               check=True, capture_output=True, text=True)
            return json.loads(r.stdout)

        response = call({"definitionId": "delegated_task", "input": {
            "task": "Review", "context": "Source", "acceptance": "Evidence"}})
        response = call(response["links"][0]["args"])
        self.assertEqual(response["workflow"]["state"], "awaiting_review")
        accept = next(link["args"] for link in response["links"] if link["rel"] == "accept")
        refused = call(accept)
        self.assertIn("error", refused)
        accepted = call({**accept, "arguments": {"evidence": "Independent fixture check passed"}})
        self.assertNotIn("error", accepted)
        self.assertEqual(accepted["workflow"]["state"], "accepted")
        self.assertEqual(accepted["context"]["verification"], "Independent fixture check passed")


if __name__ == "__main__":
    unittest.main()
