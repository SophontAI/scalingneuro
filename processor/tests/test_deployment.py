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

    def test_installer_hardens_host_python_before_first_invocation(self) -> None:
        installer = (
            PROCESSOR_ROOT / "scripts" / "install-native-on-compute.sh"
        ).read_text(encoding="utf-8")
        first_python_invocation = installer.index('if ! "$python_bin" -c')
        prefix = installer[:first_python_invocation]
        for assignment in (
            "export PYTHONNOUSERSITE=1",
            "export PYTHONDONTWRITEBYTECODE=1",
            "export PYTHONSAFEPATH=1",
        ):
            with self.subTest(assignment=assignment):
                self.assertEqual(installer.count(assignment), 1)
                self.assertIn(assignment, prefix)

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

    def test_native_runner_passes_only_explicit_controller_environment(self) -> None:
        runner = (PROCESSOR_ROOT / "slurm" / "run-processor-native.sbatch").read_text(
            encoding="utf-8"
        )
        self.assertIn("#SBATCH --export=NIL", runner)
        self.assertNotIn("SLURM_EXPORT_ENV", runner)
        self.assertNotIn("--export=ALL", runner)
        self.assertNotIn("export ENROOT_RESTRICT_DEV", runner)
        digest_invocation = runner.index('actual_controller_sha256=$("$python_bin" -')
        self.assertIn("export PYTHONSAFEPATH=1", runner[:digest_invocation])
        self.assertRegex(
            runner,
            r'"\$srun_bin" \\\n'
            r'\s+"\$env_bin" \\\n'
            r'\s+"PYTHONPATH=\$app_root" \\\n'
            r"\s+PYTHONNOUSERSITE=1 \\\n"
            r"\s+PYTHONDONTWRITEBYTECODE=1 \\\n"
            r"\s+PYTHONSAFEPATH=1 \\\n"
            r"\s+TZ=UTC \\\n"
            r'\s+"\$python_bin" -m scaling_neuro_processor',
        )

    def test_native_runner_keeps_processor_token_file_only(self) -> None:
        runner = (PROCESSOR_ROOT / "slurm" / "run-processor-native.sbatch").read_text(
            encoding="utf-8"
        )
        self.assertEqual(runner.count('--token-file "$token_file"'), 1)
        self.assertNotIn("SCALING_NEURO_PROCESSOR_TOKEN", runner)
        self.assertNotRegex(runner, r"(?:cat|head|tail|sed).*\$token_file")
        self.assertNotRegex(runner, r"(?:TOKEN|token)=\$\([^\n]*\$token_file")

    def test_legacy_runner_passes_only_enroot_device_restriction(self) -> None:
        runner = (PROCESSOR_ROOT / "slurm" / "run-processor.sbatch").read_text(
            encoding="utf-8"
        )
        self.assertIn("#SBATCH --export=NIL", runner)
        self.assertEqual(runner.count("--export=ENROOT_RESTRICT_DEV=yes"), 1)
        self.assertNotIn("--export=ALL", runner)
        self.assertNotRegex(runner, r"(?m)^export ENROOT_RESTRICT_DEV=")
        self.assertRegex(
            runner,
            r"/opt/slurm/bin/srun \\\n"
            r"\s+--export=ENROOT_RESTRICT_DEV=yes \\\n"
            r'\s+--container-image="\$image"',
        )

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
