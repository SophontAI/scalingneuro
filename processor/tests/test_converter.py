from __future__ import annotations

from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

from scaling_neuro_processor.config import Config
from scaling_neuro_processor.converter import (
    NORMALIZED_ARGUMENTS,
    SANDBOX_DCM2NIIX,
    _subprocess_environment,
    check_version,
    conversion_command,
    version_command,
)
from scaling_neuro_processor.errors import ProcessorError


class ConverterSandboxTests(unittest.TestCase):
    def config(self, **overrides) -> Config:
        values = {
            "api_url": "https://scalingneuro.com",
            "token": "controller-secret-not-for-converter",
            "work_root": Path("/private/controller-work"),
            "processor_id": "sandbox-test",
            "dcm2niix_bin": "/private/release/bin/dcm2niix",
            "native_tools_slurm_image": Path("/private/release/native-tools.sqsh"),
            "slurm_srun_bin": "/opt/slurm/bin/srun",
            "slurm_job_id": "12345",
        }
        values.update(overrides)
        return Config(**values)

    def test_rejects_unsafe_slurm_container_configuration(self) -> None:
        for overrides in (
            {"native_tools_slurm_image": Path("relative.sqsh")},
            {"native_tools_slurm_image": Path("/release/unsafe,image.sqsh")},
            {"slurm_srun_bin": "srun"},
            {"native_tools_slurm_image": None},
            {"slurm_job_id": None},
            {"slurm_job_id": "0"},
            {"slurm_job_id": "0123"},
            {"slurm_job_id": "-1"},
            {"slurm_job_id": " 123"},
            {"slurm_job_id": "123_4"},
            {"slurm_job_id": "1" * 21},
        ):
            with self.subTest(overrides=overrides):
                with self.assertRaisesRegex(
                    ProcessorError, "CONVERTER_SANDBOX_CONFIGURATION_INVALID"
                ):
                    self.config(**overrides)

    def test_version_runs_inside_minimal_container(self) -> None:
        config = self.config()
        command = version_command(config)
        self.assertEqual(command[0], "/opt/slurm/bin/srun")
        for required in (
            "--jobid=12345",
            "--overlap",
            "--nodes=1",
            "--ntasks=1",
            "--container-image=/private/release/native-tools.sqsh",
            "--container-readonly",
            "--no-container-mount-home",
            "--no-container-remap-root",
            "--no-container-entrypoint",
            "--container-workdir=/tmp",
        ):
            self.assertIn(required, command)
        self.assertNotIn(config.dcm2niix_bin, command)
        self.assertNotIn(str(config.work_root), command)
        self.assertEqual(command[-2:], [SANDBOX_DCM2NIIX, "--version"])
        with tempfile.TemporaryDirectory() as directory:
            runtime = Path(directory) / "enroot"
            config = self.config(enroot_runtime_root=runtime)
            environment = _subprocess_environment(config, config.work_root)
            self.assertEqual(environment["ENROOT_CACHE_PATH"], str(runtime / "cache"))
            self.assertEqual(environment["ENROOT_DATA_PATH"], str(runtime / "data"))
            self.assertEqual(
                environment["ENROOT_RUNTIME_PATH"], str(runtime / "runtime")
            )
            self.assertEqual(environment["ENROOT_TEMP_PATH"], str(runtime / "tmp"))
            self.assertEqual(environment["ENROOT_RESTRICT_DEV"], "yes")
            self.assertEqual(environment["HOME"], "/tmp")
            self.assertEqual(environment["LANG"], "C")
            self.assertEqual(environment["LC_ALL"], "C")
            self.assertEqual(environment["PATH"], "/opt/slurm/bin:/usr/bin:/bin")
            self.assertEqual(environment["TZ"], "UTC")
            self.assertEqual(environment["XDG_RUNTIME_DIR"], str(runtime / "runtime"))
            self.assertNotIn(config.token, environment.values())
            self.assertNotIn(str(config.work_root), environment.values())
            self.assertFalse(any(key.startswith("SLURM_") for key in environment))

    def test_conversion_binds_only_input_readonly_and_output_writable(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            dicom = root / "dicom"
            output = root / "output"
            dicom.mkdir()
            output.mkdir()
            config = self.config()
            command = conversion_command(config, dicom, output)

        self.assertIn(
            (
                f"--container-mounts={dicom.resolve()}:/input:ro+rprivate,"
                f"{output.resolve()}:/output:rw+rprivate"
            ),
            command,
        )
        self.assertEqual(
            command[-(len(NORMALIZED_ARGUMENTS) + 4) :],
            [
                SANDBOX_DCM2NIIX,
                *NORMALIZED_ARGUMENTS,
                "-o",
                "/output",
                "/input",
            ],
        )
        self.assertNotIn(config.dcm2niix_bin, command)
        self.assertNotIn(str(config.work_root), command)
        self.assertNotIn(config.token, command)

    @patch("scaling_neuro_processor.converter.subprocess.run")
    def test_official_version_probe_exit_status_is_accepted(self, run) -> None:
        run.return_value = subprocess.CompletedProcess(
            [], 3, stdout=b"dcm2niix version v1.0.20260416\n"
        )
        with tempfile.TemporaryDirectory() as directory:
            config = self.config(enroot_runtime_root=Path(directory) / "enroot")
            self.assertEqual(check_version(config), "v1.0.20260416")

            run.return_value = subprocess.CompletedProcess(
                [], 2, stdout=b"dcm2niix version v1.0.20260416\n"
            )
            with self.assertRaisesRegex(ProcessorError, "DCM2NIIX_VERSION_MISMATCH"):
                check_version(config)

    def test_direct_container_command_remains_normalized(self) -> None:
        config = self.config(native_tools_slurm_image=None, slurm_job_id=None)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            dicom = root / "dicom"
            output = root / "output"
            dicom.mkdir()
            output.mkdir()
            self.assertEqual(
                conversion_command(config, dicom, output),
                [
                    config.dcm2niix_bin,
                    *NORMALIZED_ARGUMENTS,
                    "-o",
                    str(output),
                    str(dicom),
                ],
            )


if __name__ == "__main__":
    unittest.main()
