from __future__ import annotations

import hashlib
import hmac
import json
from pathlib import Path
import tempfile
import unittest
import uuid

from launcher.server import LaunchError, ReceiptStore, verify_request


class LauncherRequestTests(unittest.TestCase):
    def setUp(self) -> None:
        self.key = bytes(range(32))
        self.now = 1_700_000_000
        self.nonce = str(uuid.UUID("12345678-1234-4234-9234-123456789abc"))
        self.upload_id = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee"
        self.body = json.dumps(
            {"event": "dicom-upload-committed", "upload_id": self.upload_id},
            separators=(",", ":"),
            sort_keys=True,
        ).encode()
        signed = f"{self.now}\n{self.nonce}\n".encode() + self.body
        self.signature = "v1=" + hmac.new(self.key, signed, hashlib.sha256).hexdigest()

    def test_accepts_exact_signed_upload_event(self) -> None:
        self.assertEqual(
            verify_request(
                self.body,
                str(self.now),
                self.nonce,
                self.signature,
                self.key,
                self.now,
            ),
            self.upload_id,
        )

    def test_rejects_tampering_and_expired_requests(self) -> None:
        with self.assertRaises(LaunchError):
            verify_request(
                self.body + b" ",
                str(self.now),
                self.nonce,
                self.signature,
                self.key,
                self.now,
            )
        with self.assertRaises(LaunchError):
            verify_request(
                self.body,
                str(self.now),
                self.nonce,
                self.signature,
                self.key,
                self.now + 301,
            )

    def test_receipts_deduplicate_upload_ids(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            store = ReceiptStore(Path(directory) / "receipts.sqlite3")
            self.assertFalse(store.contains(self.upload_id))
            store.record(self.upload_id)
            store.record(self.upload_id)
            self.assertTrue(store.contains(self.upload_id))


if __name__ == "__main__":
    unittest.main()
