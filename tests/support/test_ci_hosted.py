import os
from pathlib import Path
import subprocess
import unittest


class HostedGuardTests(unittest.TestCase):
    def test_refuses_local_and_self_hosted_execution_before_provisioning(self):
        for actions, environment, runner_os in (
            ("", "", ""),
            ("false", "github-hosted", "Linux"),
            ("true", "self-hosted", "Linux"),
            ("true", "github-hosted", "Windows"),
        ):
            with self.subTest(actions=actions, environment=environment, os=runner_os):
                env = {
                    "PATH": os.environ["PATH"],
                    "GITHUB_ACTIONS": actions,
                    "RUNNER_ENVIRONMENT": environment,
                    "RUNNER_OS": runner_os,
                }
                result = subprocess.run(
                    ["bash", str(Path(__file__).with_name("ci_e2e.sh"))],
                    env=env,
                    capture_output=True,
                    text=True,
                    timeout=5,
                )
                self.assertEqual(result.returncode, 1)
                self.assertIn("refusing pool provisioning", result.stderr)
                self.assertEqual(result.stdout, "")


if __name__ == "__main__":
    unittest.main()
