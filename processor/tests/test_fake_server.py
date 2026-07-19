from __future__ import annotations

from contextlib import contextmanager
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import tempfile
from threading import Event, Thread
import unittest

from scaling_neuro_processor.api import ControlPlane
from scaling_neuro_processor.config import Config
from scaling_neuro_processor.errors import ApiFailure, InvalidArchive
from scaling_neuro_processor.pipeline import process_job

from tests.helpers import (
    ARCHIVE_ID,
    TEST_DISK_RESERVE_BYTES,
    archive_manifest,
    canonical_json,
    fake_converter,
    gzip_bytes,
    legacy_sidecar,
    make_archive,
    make_dicom,
    nifti_bytes,
)


class State:
    def __init__(self, job: dict, objects: dict[str, bytes]) -> None:
        self.job = job
        self.claimed = False
        self.claim_request: dict | None = None
        self.objects = objects
        self.uploaded: dict[str, bytes] = {}
        self.output_request: dict | None = None
        self.complete_request: dict | None = None
        self.fail_request: dict | None = None
        self.object_authorization: list[str | None] = []
        self.output_failures = 0


def handler_for(state: State):
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, _format, *_args):  # noqa: ANN001
            return

        def send_json(self, status: int, value: dict) -> None:
            raw = canonical_json(value)
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(raw)))
            self.end_headers()
            self.wfile.write(raw)

        def read_json(self) -> dict:
            size = int(self.headers.get("Content-Length", "0"))
            return json.loads(self.rfile.read(size))

        def do_POST(self) -> None:
            self.assert_api_auth()
            body = self.read_json()
            if self.path == "/v1/processor/jobs/claim":
                state.claim_request = body
                if state.claimed:
                    self.send_response(204)
                    self.end_headers()
                else:
                    state.claimed = True
                    self.send_json(200, state.job)
                return
            if self.path.endswith("/heartbeat"):
                self.send_json(200, {})
                return
            if self.path.endswith("/outputs"):
                if state.output_failures:
                    state.output_failures -= 1
                    self.send_json(503, {"code": "TEMPORARY"})
                    return
                state.output_request = body
                grants = []
                for item in body["outputs"]:
                    grants.append(
                        {
                            "kind": item["kind"],
                            "url": f"http://127.0.0.1:{self.server.server_port}/put/{item['kind']}",
                            "expires_at": "2030-01-01T00:00:00Z",
                            "headers": {"x-test-sha256": item["sha256"]},
                        }
                    )
                self.send_json(200, {"outputs": grants})
                return
            if self.path.endswith("/complete"):
                state.complete_request = body
                self.send_json(200, {})
                return
            if self.path.endswith("/fail"):
                state.fail_request = body
                self.send_json(200, {})
                return
            self.send_error(404)

        def do_GET(self) -> None:
            state.object_authorization.append(self.headers.get("Authorization"))
            raw = state.objects.get(self.path)
            if raw is None:
                self.send_error(404)
                return
            self.send_response(200)
            self.send_header("Content-Length", str(len(raw)))
            self.end_headers()
            self.wfile.write(raw)

        def do_PUT(self) -> None:
            state.object_authorization.append(self.headers.get("Authorization"))
            size = int(self.headers["Content-Length"])
            raw = self.rfile.read(size)
            kind = self.path.rsplit("/", 1)[-1]
            expected = next(
                item for item in state.output_request["outputs"] if item["kind"] == kind
            )
            if (
                len(raw) != expected["size_bytes"]
                or hashlib.sha256(raw).hexdigest() != expected["sha256"]
            ):
                self.send_error(400)
                return
            state.uploaded[kind] = raw
            self.send_response(200)
            self.send_header("Content-Length", "0")
            self.end_headers()

        def assert_api_auth(self) -> None:
            if self.headers.get("Authorization") != "Bearer processor-test-token":
                self.send_error(401)
                raise AssertionError("missing processor bearer token")

    return Handler


@contextmanager
def fake_server(state: State):
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler_for(state))
    thread = Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield server
    finally:
        server.shutdown()
        thread.join()
        server.server_close()


def descriptor(server: ThreadingHTTPServer, path: str, raw: bytes, **extra) -> dict:
    return {
        "url": f"http://127.0.0.1:{server.server_port}{path}",
        "size_bytes": len(raw),
        "sha256": hashlib.sha256(raw).hexdigest(),
        **extra,
    }


class FakeServerIntegrationTests(unittest.TestCase):
    def config(
        self, root: Path, server: ThreadingHTTPServer, converter: Path
    ) -> Config:
        return Config(
            api_url=f"http://127.0.0.1:{server.server_port}",
            token="processor-test-token",
            work_root=root / "work",
            processor_id="integration-test",
            dcm2niix_bin=str(converter),
            disk_reserve_bytes=TEST_DISK_RESERVE_BYTES,
            allow_insecure_http=True,
            allowed_object_hosts=("127.0.0.1",),
            max_jobs=1,
        )

    def test_dicom_job_downloads_converts_uploads_and_completes(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            dicom = make_dicom(root / "source.dcm")
            archive = make_archive(
                root / "series.tar.zst", dicom, archive_manifest(dicom)
            )
            converter = root / "fake-dcm2niix"
            fake_converter(converter)
            state = State({}, {"/objects/archive": archive})
            with fake_server(state) as server:
                state.job = {
                    "schema_version": "1.0.0",
                    "job_id": "job-1",
                    "upload_id": "upload-1",
                    "bundle_id": ARCHIVE_ID,
                    "series_archive_id": ARCHIVE_ID,
                    "series_id": "b" * 24,
                    "attempt": 1,
                    "lease_token": "lease-token",
                    "lease_expires_at": "2030-01-01T00:00:00Z",
                    "input_format": "dicom-series-v1",
                    "input": {
                        "format": "dicom-tar-zstd",
                        "dicom_count": 1,
                        **descriptor(server, "/objects/archive", archive),
                    },
                }
                config = self.config(root, server, converter)
                api = ControlPlane(config, sleep=lambda _: None)
                job = api.claim()
                self.assertIsNotNone(job)
                active = Event()
                active.set()
                process_job(config, api, job, active)

            self.assertEqual(
                set(state.uploaded), {"nifti", "sidecar", "processing_manifest"}
            )
            self.assertEqual(
                state.claim_request,
                {"processor_id": "integration-test", "lease_seconds": 900},
            )
            self.assertTrue(
                state.complete_request["validation"]["archive_sha256_verified"]
            )
            self.assertTrue(
                state.complete_request["validation"]["functional_epi_confirmed"]
            )
            self.assertEqual(state.complete_request["validation"]["dicom_count"], 1)
            self.assertEqual(
                state.complete_request["dcm2niix_version"], "v1.0.20260416"
            )
            processing = json.loads(state.uploaded["processing_manifest"])
            self.assertNotIn("job_id", processing)
            self.assertEqual(
                processing["input"]["sha256"], hashlib.sha256(archive).hexdigest()
            )
            self.assertEqual(state.object_authorization, [None, None, None, None])

    def test_dicom_download_hash_mismatch_is_terminal_archive_failure(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            dicom = make_dicom(root / "source.dcm")
            archive = make_archive(
                root / "series.tar.zst", dicom, archive_manifest(dicom)
            )
            corrupted = bytearray(archive)
            corrupted[len(corrupted) // 2] ^= 1
            converter = root / "fake-dcm2niix"
            fake_converter(converter)
            state = State({}, {"/objects/archive": bytes(corrupted)})
            with fake_server(state) as server:
                state.job = {
                    "schema_version": "1.0.0",
                    "job_id": "job-corrupt-download",
                    "upload_id": "upload-corrupt-download",
                    "bundle_id": ARCHIVE_ID,
                    "series_archive_id": ARCHIVE_ID,
                    "series_id": "b" * 24,
                    "attempt": 1,
                    "lease_token": "lease-token",
                    "lease_expires_at": "2030-01-01T00:00:00Z",
                    "input_format": "dicom-series-v1",
                    "input": {
                        "format": "dicom-tar-zstd",
                        "dicom_count": 1,
                        **descriptor(server, "/objects/archive", archive),
                    },
                }
                config = self.config(root, server, converter)
                api = ControlPlane(config, sleep=lambda _: None)
                job = api.claim()
                active = Event()
                active.set()
                with self.assertRaisesRegex(
                    InvalidArchive, "ARCHIVE_DOWNLOAD_INTEGRITY_MISMATCH"
                ):
                    process_job(config, api, job, active)

            self.assertEqual(state.uploaded, {})
            self.assertFalse(Path(str(converter) + ".count").exists())

    def test_legacy_job_validates_in_place_without_reupload(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            raw_nifti = nifti_bytes()
            compressed = gzip_bytes(raw_nifti)
            sidecar = canonical_json(legacy_sidecar(raw_nifti, compressed))
            converter = root / "fake-dcm2niix"
            fake_converter(converter)
            state = State(
                {},
                {"/objects/legacy.nii.gz": compressed, "/objects/legacy.json": sidecar},
            )
            with fake_server(state) as server:
                state.job = {
                    "schema_version": "1.0.0",
                    "job_id": "legacy-job-1",
                    "upload_id": "legacy-upload-1",
                    "bundle_id": ARCHIVE_ID,
                    "series_id": "b" * 24,
                    "attempt": 1,
                    "lease_token": "lease-token",
                    "lease_expires_at": "2030-01-01T00:00:00Z",
                    "input_format": "nifti-v1",
                    "input": {
                        "nifti": descriptor(
                            server,
                            "/objects/legacy.nii.gz",
                            compressed,
                            uncompressed_sha256=hashlib.sha256(raw_nifti).hexdigest(),
                            filename="legacy.nii.gz",
                        ),
                        "sidecar": descriptor(
                            server,
                            "/objects/legacy.json",
                            sidecar,
                            filename="legacy.json",
                        ),
                    },
                }
                config = self.config(root, server, converter)
                api = ControlPlane(config, sleep=lambda _: None)
                job = api.claim()
                active = Event()
                active.set()
                process_job(config, api, job, active)

            self.assertEqual(state.uploaded, {})
            self.assertEqual(state.complete_request["outputs"], [])
            self.assertEqual(
                state.complete_request["validation"],
                {
                    "nifti_sha256_verified": True,
                    "nifti_uncompressed_sha256_verified": True,
                    "sidecar_sha256_verified": True,
                    "nifti_header_valid": True,
                    "sidecar_valid": True,
                    "nifti_sidecar_consistent": True,
                },
            )
            self.assertEqual(state.object_authorization, [None, None])

    def test_retry_reuses_deterministic_local_outputs_without_reconversion(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            dicom = make_dicom(root / "source.dcm")
            archive = make_archive(
                root / "series.tar.zst", dicom, archive_manifest(dicom)
            )
            converter = root / "fake-dcm2niix"
            fake_converter(converter)
            state = State({}, {"/objects/archive": archive})
            state.output_failures = 1
            with fake_server(state) as server:
                state.job = {
                    "schema_version": "1.0.0",
                    "job_id": "job-retry",
                    "upload_id": "upload-1",
                    "bundle_id": ARCHIVE_ID,
                    "series_archive_id": ARCHIVE_ID,
                    "series_id": "b" * 24,
                    "attempt": 1,
                    "lease_token": "lease-token",
                    "lease_expires_at": "2030-01-01T00:00:00Z",
                    "input_format": "dicom-series-v1",
                    "input": {
                        "format": "dicom-tar-zstd",
                        "dicom_count": 1,
                        **descriptor(server, "/objects/archive", archive),
                    },
                }
                config = self.config(root, server, converter)
                api_without_retry = ControlPlane(
                    config, sleep=lambda _: None, attempts=1
                )
                job = api_without_retry.claim()
                active = Event()
                active.set()
                with self.assertRaises(ApiFailure):
                    process_job(config, api_without_retry, job, active)
                self.assertEqual((Path(str(converter) + ".count")).read_text(), "1")
                process_job(config, api_without_retry, job, active)
                self.assertEqual((Path(str(converter) + ".count")).read_text(), "1")
            self.assertIsNotNone(state.complete_request)


if __name__ == "__main__":
    unittest.main()
