from __future__ import annotations

import hashlib
import io
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock

from scaling_neuro_processor.config import Config
from scaling_neuro_processor.errors import ProcessorError
from scaling_neuro_processor.models import Download, OutputFile, PutGrant
from scaling_neuro_processor.transport import ObjectTransport


class _Response(io.BytesIO):
    status = 200

    def __enter__(self):
        return self

    def __exit__(self, _exc_type, _exc, _traceback) -> None:
        self.close()


class ObjectTransportTests(unittest.TestCase):
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

    def test_download_uses_object_timeout_not_control_plane_timeout(self) -> None:
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

            transport.download(descriptor, root / "archive.tar.zst")

            self.assertEqual(transport._opener.open.call_args.kwargs["timeout"], 4321)

    def test_upload_uses_object_timeout_not_control_plane_timeout(self) -> None:
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

            transport.upload(output, grant)

            self.assertEqual(transport._opener.open.call_args.kwargs["timeout"], 5432)

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


if __name__ == "__main__":
    unittest.main()
