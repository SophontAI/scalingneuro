from __future__ import annotations

from pathlib import Path
import re
import unittest


PROCESSOR_ROOT = Path(__file__).resolve().parents[1]


class NativeDeploymentContractTests(unittest.TestCase):
    def test_installer_and_runner_share_controller_digest_contract(self) -> None:
        installer = (
            PROCESSOR_ROOT / "scripts" / "install-native-on-compute.sh"
        ).read_text(encoding="utf-8")
        runner = (PROCESSOR_ROOT / "slurm" / "run-processor-native.sbatch").read_text(
            encoding="utf-8"
        )
        pattern = re.compile(
            r"<<'CONTROLLER_DIGEST_PY'\n(?P<body>.*?)\nCONTROLLER_DIGEST_PY",
            re.DOTALL,
        )
        installer_digest = pattern.search(installer)
        runner_digest = pattern.search(runner)
        self.assertIsNotNone(installer_digest)
        self.assertIsNotNone(runner_digest)
        assert installer_digest is not None
        assert runner_digest is not None
        self.assertEqual(installer_digest.group("body"), runner_digest.group("body"))
        field = "controller_source_sha256"
        self.assertIn(f'"{field}=$source_controller_sha256"', installer)
        self.assertIn(f'grep -Fx "{field}=$source_controller_sha256"', installer)
        self.assertIn(f"s/^{field}=//p", runner)

    def test_nested_tools_receive_no_controller_or_slurm_environment(self) -> None:
        sandbox = (PROCESSOR_ROOT / "scaling_neuro_processor" / "sandbox.py").read_text(
            encoding="utf-8"
        )
        self.assertNotIn('os.environ["SLURM_', sandbox)
        self.assertNotIn("config.token", sandbox)

    def test_native_runner_passes_parent_allocation_explicitly(self) -> None:
        runner = (PROCESSOR_ROOT / "slurm" / "run-processor-native.sbatch").read_text(
            encoding="utf-8"
        )
        sandbox = (PROCESSOR_ROOT / "scaling_neuro_processor" / "sandbox.py").read_text(
            encoding="utf-8"
        )
        self.assertIn('--slurm-job-id "$SLURM_JOB_ID"', runner)
        self.assertIn('f"--jobid={job_id}"', sandbox)

    def test_installer_version_probes_remain_in_parent_allocation(self) -> None:
        installer = (
            PROCESSOR_ROOT / "scripts" / "install-native-on-compute.sh"
        ).read_text(encoding="utf-8")
        self.assertRegex(
            installer,
            r'validate_sandbox_version \\\n\s+"\$release" \\\n\s+dcm2niix',
        )
        self.assertRegex(
            installer,
            r'validate_sandbox_version \\\n\s+"\$release" \\\n\s+zstd',
        )
        self.assertIn('--jobid="$SLURM_JOB_ID"', installer)
        self.assertIn(
            '"sandboxed $tool_name version probe returned $actual_status; '
            'expected $expected_status"',
            installer,
        )


if __name__ == "__main__":
    unittest.main()
