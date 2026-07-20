from __future__ import annotations

import hashlib
import io
from pathlib import Path
import tempfile
from threading import Event
from typing import Any
import unittest
from unittest.mock import Mock, patch

from scaling_neuro_processor.config import Config
from scaling_neuro_processor.errors import LeaseLost, ProcessorError
from scaling_neuro_processor.models import Download, OutputFile, PutGrant
from scaling_neuro_processor.transport import ObjectTransport


class _Response(io.BytesIO):
    status = 200

    def __enter__(self):
        return self

    def __exit__(self, _exc_type, _exc, _traceback) -> None:
        self.close()


class ObjectTransportTests(unittest.TestCase):
    def active_lease(self) -> Event:
        active = Event()
        active.set()
        return active

    def config(self, root: Path, *, object_timeout: int = 3600) -> Config:
        return Config(
            api_url="https://api.example",
            token="test",
            work_root=root / "work",
            processor_id="test",
            request_timeout_seconds=120,
            object_transfer_timeout_seconds=object_timeout,
            allowed_object_hosts=("objects.example",),
        )

    def test_download_caps_socket_wait_below_total_object_deadline(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            payload = b"large-object-fixture"
            transport = ObjectTransport(self.config(root, object_timeout=4321))
            transport._opener = Mock()
            transport._opener.open.return_value = _Response(payload)
            descriptor = Download(
                url="https://objects.example/archive",
                size_bytes=len(payload),
                sha256=hashlib.sha256(payload).hexdigest(),
                headers={},
            )

            transport.download(
                descriptor, root / "archive.tar.zst", self.active_lease()
            )

            self.assertEqual(transport._opener.open.call_args.kwargs["timeout"], 300)

    def test_upload_caps_socket_wait_below_total_object_deadline(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            payload = b"derived-object-fixture"
            path = root / "derived.nii.gz"
            path.write_bytes(payload)
            transport = ObjectTransport(self.config(root, object_timeout=5432))
            transport._opener = Mock()
            transport._opener.open.return_value = _Response()
            output = OutputFile(
                kind="nifti",
                path=str(path),
                size_bytes=len(payload),
                sha256=hashlib.sha256(payload).hexdigest(),
                content_type="application/gzip",
            )
            grant = PutGrant(
                kind="nifti",
                url="https://objects.example/derived",
                expires_at="2030-01-01T00:00:00Z",
                headers={},
            )

            transport.upload(output, grant, self.active_lease())

            self.assertEqual(transport._opener.open.call_args.kwargs["timeout"], 300)

    def test_upload_enforces_a_total_wall_clock_deadline(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            payload = b"derived-object-fixture"
            path = root / "derived.nii.gz"
            path.write_bytes(payload)
            transport = ObjectTransport(self.config(root, object_timeout=300))
            output = OutputFile(
                kind="nifti",
                path=str(path),
                size_bytes=len(payload),
                sha256=hashlib.sha256(payload).hexdigest(),
                content_type="application/gzip",
            )
            grant = PutGrant(
                kind="nifti",
                url="https://objects.example/derived",
                expires_at="2030-01-01T00:00:00Z",
                headers={},
            )

            def consume_body(request: Any, *, timeout: int) -> _Response:
                self.assertEqual(timeout, 300)
                request.data.read()
                return _Response()

            transport._opener = Mock()
            transport._opener.open.side_effect = consume_body
            with (
                patch(
                    "scaling_neuro_processor.transport.monotonic",
                    side_effect=[0.0, 0.0, 301.0],
                ),
                self.assertRaisesRegex(ProcessorError, "OBJECT_UPLOAD_FAILED") as raised,
            ):
                transport.upload(output, grant, self.active_lease())

            self.assertTrue(raised.exception.retryable)

    def test_download_integrity_disagreement_is_retryable(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            payload = b"cleanly-truncated-object"
            transport = ObjectTransport(self.config(root))
            transport._opener = Mock()
            transport._opener.open.return_value = _Response(payload)
            descriptor = Download(
                url="https://objects.example/archive",
                size_bytes=len(payload) + 1,
                sha256=hashlib.sha256(payload + b"x").hexdigest(),
                headers={},
            )
            destination = root / "archive.tar.zst"

            with self.assertRaisesRegex(
                ProcessorError, "OBJECT_DOWNLOAD_INTEGRITY_MISMATCH"
            ) as raised:
                transport.download(descriptor, destination, self.active_lease())

            self.assertTrue(raised.exception.retryable)
            self.assertFalse(destination.exists())

    def test_download_enforces_a_total_wall_clock_deadline(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            payload = b"slow-progress-object"
            transport = ObjectTransport(self.config(root, object_timeout=300))
            transport._opener = Mock()
            transport._opener.open.return_value = _Response(payload)
            descriptor = Download(
                url="https://objects.example/archive",
                size_bytes=len(payload),
                sha256=hashlib.sha256(payload).hexdigest(),
                headers={},
            )

            with (
                patch(
                    "scaling_neuro_processor.transport.monotonic",
                    side_effect=[0.0, 0.0, 301.0],
                ),
                self.assertRaisesRegex(ProcessorError, "OBJECT_DOWNLOAD_FAILED") as raised,
            ):
                transport.download(
                    descriptor, root / "archive.tar.zst", self.active_lease()
                )

            self.assertTrue(raised.exception.retryable)

    def test_object_timeout_is_bounded_independently(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config = self.config(root)
            self.assertEqual(config.request_timeout_seconds, 120)
            self.assertEqual(config.object_transfer_timeout_seconds, 3600)
            for invalid in (299, 86_401):
                with self.subTest(invalid=invalid):
                    with self.assertRaisesRegex(
                        ProcessorError, "OBJECT_TRANSFER_TIMEOUT_INVALID"
                    ):
                        self.config(root, object_timeout=invalid)

    def test_lease_loss_cancels_download_between_chunks(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            payload = b"streamed-object"
            active = self.active_lease()

            class LeaseCancellingResponse(_Response):
                def read1(self, size: int = -1) -> bytes:
                    chunk = super().read1(size)
                    active.clear()
                    return chunk

            transport = ObjectTransport(self.config(root))
            transport._opener = Mock()
            transport._opener.open.return_value = LeaseCancellingResponse(payload)
            descriptor = Download(
                url="https://objects.example/archive",
                size_bytes=len(payload),
                sha256=hashlib.sha256(payload).hexdigest(),
                headers={},
            )
            destination = root / "archive.tar.zst"

            with self.assertRaises(LeaseLost):
                transport.download(descriptor, destination, active)

            self.assertFalse(destination.exists())

    def test_lease_loss_cancels_upload_reader(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            payload = b"derived-object-fixture"
            path = root / "derived.nii.gz"
            path.write_bytes(payload)
            active = self.active_lease()
            transport = ObjectTransport(self.config(root))
            output = OutputFile(
                kind="nifti",
                path=str(path),
                size_bytes=len(payload),
                sha256=hashlib.sha256(payload).hexdigest(),
                content_type="application/gzip",
            )
            grant = PutGrant(
                kind="nifti",
                url="https://objects.example/derived",
                expires_at="2030-01-01T00:00:00Z",
                headers={},
            )

            def cancel_before_consuming(request: Any, *, timeout: int) -> _Response:
                self.assertEqual(timeout, 300)
                active.clear()
                request.data.read()
                return _Response()

            transport._opener = Mock()
            transport._opener.open.side_effect = cancel_before_consuming

            with self.assertRaises(LeaseLost):
                transport.upload(output, grant, active)

    def test_86400_second_transfer_allows_continuous_slow_progress(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            payload = b"slow"
            active = self.active_lease()
            transport = ObjectTransport(self.config(root, object_timeout=86_400))
            transport._opener = Mock()
            transport._opener.open.return_value = _Response(payload)
            descriptor = Download(
                url="https://objects.example/archive",
                size_bytes=len(payload),
                sha256=hashlib.sha256(payload).hexdigest(),
                headers={},
            )
            destination = root / "archive.tar.zst"

            with (
                patch("scaling_neuro_processor.transport.CHUNK_BYTES", 1),
                patch(
                    "scaling_neuro_processor.transport.monotonic",
                    side_effect=[
                        0.0,
                        0.0,
                        10_000.0,
                        20_000.0,
                        30_000.0,
                        40_000.0,
                        50_000.0,
                        60_000.0,
                        70_000.0,
                        80_000.0,
                        86_000.0,
                    ],
                ),
            ):
                transport.download(descriptor, destination, active)

            self.assertEqual(destination.read_bytes(), payload)


if __name__ == "__main__":
    unittest.main()
