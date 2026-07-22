from __future__ import annotations

import argparse
import base64
import binascii
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import hmac
import json
import os
from pathlib import Path
import re
import sqlite3
import stat
import subprocess
import time
import uuid


MAX_BODY_BYTES = 4096
MAX_CLOCK_SKEW_SECONDS = 300
SIGNATURE_RE = re.compile(r"v1=([0-9a-f]{64})")


class LaunchError(Exception):
    def __init__(self, status: int, code: str) -> None:
        super().__init__(code)
        self.status = status
        self.code = code


def read_hmac_key(path: Path) -> bytes:
    file_stat = path.stat()
    if not stat.S_ISREG(file_stat.st_mode) or file_stat.st_mode & 0o077:
        raise ValueError("HMAC key file must be a private regular file")
    try:
        key = base64.b64decode(path.read_text(encoding="ascii").strip(), validate=True)
    except (OSError, UnicodeError, binascii.Error) as error:
        raise ValueError("HMAC key file is invalid") from error
    if len(key) != 32:
        raise ValueError("HMAC key must contain exactly 32 bytes")
    return key


def verify_request(
    body: bytes,
    timestamp_value: str | None,
    nonce_value: str | None,
    signature_value: str | None,
    key: bytes,
    now: int | None = None,
) -> str:
    if not timestamp_value or not timestamp_value.isascii() or not timestamp_value.isdigit():
        raise LaunchError(401, "invalid_signature")
    timestamp = int(timestamp_value)
    if abs((int(time.time()) if now is None else now) - timestamp) > MAX_CLOCK_SKEW_SECONDS:
        raise LaunchError(401, "expired_signature")
    try:
        nonce = uuid.UUID(nonce_value or "")
    except ValueError as error:
        raise LaunchError(401, "invalid_signature") from error
    if nonce.version != 4 or str(nonce) != nonce_value:
        raise LaunchError(401, "invalid_signature")
    match = SIGNATURE_RE.fullmatch(signature_value or "")
    if not match:
        raise LaunchError(401, "invalid_signature")
    expected = hmac.new(
        key,
        timestamp_value.encode("ascii") + b"\n" + nonce_value.encode("ascii") + b"\n" + body,
        "sha256",
    ).hexdigest()
    if not hmac.compare_digest(expected, match.group(1)):
        raise LaunchError(401, "invalid_signature")
    try:
        payload = json.loads(body)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise LaunchError(400, "invalid_request") from error
    if not isinstance(payload, dict) or set(payload) != {"event", "upload_id"}:
        raise LaunchError(400, "invalid_request")
    if payload["event"] != "dicom-upload-committed":
        raise LaunchError(400, "invalid_request")
    try:
        upload_id = uuid.UUID(payload["upload_id"])
    except (ValueError, TypeError, AttributeError) as error:
        raise LaunchError(400, "invalid_request") from error
    if str(upload_id) != payload["upload_id"]:
        raise LaunchError(400, "invalid_request")
    return payload["upload_id"]


class ReceiptStore:
    def __init__(self, path: Path) -> None:
        self.path = path
        with self.connect() as connection:
            connection.execute(
                """CREATE TABLE IF NOT EXISTS launch_receipts (
                       upload_id TEXT PRIMARY KEY,
                       launched_at INTEGER NOT NULL
                   )"""
            )

    def connect(self) -> sqlite3.Connection:
        connection = sqlite3.connect(self.path, timeout=30)
        connection.execute("PRAGMA busy_timeout = 30000")
        return connection

    def contains(self, upload_id: str) -> bool:
        with self.connect() as connection:
            return connection.execute(
                "SELECT 1 FROM launch_receipts WHERE upload_id = ?", (upload_id,)
            ).fetchone() is not None

    def record(self, upload_id: str) -> None:
        with self.connect() as connection:
            connection.execute(
                "INSERT OR IGNORE INTO launch_receipts (upload_id, launched_at) VALUES (?, ?)",
                (upload_id, int(time.time())),
            )


class LaunchServer(ThreadingHTTPServer):
    key: bytes
    launcher: Path
    receipts: ReceiptStore


class LaunchHandler(BaseHTTPRequestHandler):
    server: LaunchServer

    def do_POST(self) -> None:  # noqa: N802
        try:
            if self.path != "/v1/launch":
                raise LaunchError(404, "not_found")
            if self.headers.get_content_type() != "application/json":
                raise LaunchError(415, "invalid_content_type")
            try:
                length = int(self.headers.get("content-length", ""))
            except ValueError as error:
                raise LaunchError(400, "invalid_request") from error
            if length < 1 or length > MAX_BODY_BYTES:
                raise LaunchError(413, "request_too_large")
            body = self.rfile.read(length)
            if len(body) != length:
                raise LaunchError(400, "invalid_request")
            upload_id = verify_request(
                body,
                self.headers.get("x-scaling-neuro-timestamp"),
                self.headers.get("x-scaling-neuro-nonce"),
                self.headers.get("x-scaling-neuro-signature"),
                self.server.key,
            )
            if self.server.receipts.contains(upload_id):
                self.respond(202, {"status": "already_launched"})
                return
            try:
                subprocess.run(
                    [str(self.server.launcher), upload_id],
                    check=True,
                    stdin=subprocess.DEVNULL,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                    timeout=45,
                    env={
                        "HOME": "/data/paul",
                        "LANG": "C.UTF-8",
                        "PATH": "/opt/slurm/bin:/usr/local/bin:/usr/bin:/bin",
                    },
                )
            except (OSError, subprocess.SubprocessError) as error:
                raise LaunchError(503, "launcher_unavailable") from error
            self.server.receipts.record(upload_id)
            self.respond(202, {"status": "launched"})
        except LaunchError as error:
            self.respond(error.status, {"error": {"code": error.code}})

    def do_GET(self) -> None:  # noqa: N802
        if self.path == "/health":
            self.respond(200, {"status": "ok"})
        else:
            self.respond(404, {"error": {"code": "not_found"}})

    def respond(self, status: int, payload: dict[str, object]) -> None:
        body = (json.dumps(payload, separators=(",", ":"), sort_keys=True) + "\n").encode()
        self.send_response(status)
        self.send_header("cache-control", "no-store")
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.send_header("x-content-type-options", "nosniff")
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, format: str, *args: object) -> None:
        return


def parser() -> argparse.ArgumentParser:
    value = argparse.ArgumentParser(prog="scaling-neuro-cluster-launcher")
    value.add_argument("--bind", default="127.0.0.1")
    value.add_argument("--port", type=int, default=8788)
    value.add_argument("--key-file", type=Path, required=True)
    value.add_argument("--launcher", type=Path, required=True)
    value.add_argument("--state-db", type=Path, required=True)
    return value


def main(argv: list[str] | None = None) -> None:
    os.umask(0o077)
    args = parser().parse_args(argv)
    if not args.launcher.is_file() or not os.access(args.launcher, os.X_OK):
        raise SystemExit("launcher must be an executable regular file")
    args.state_db.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    server = LaunchServer((args.bind, args.port), LaunchHandler)
    server.key = read_hmac_key(args.key_file)
    server.launcher = args.launcher
    server.receipts = ReceiptStore(args.state_db)
    server.serve_forever()


if __name__ == "__main__":
    main()
