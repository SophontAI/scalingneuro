from __future__ import annotations

import json
import random
import time
from typing import Any, Callable, Literal
from urllib.error import HTTPError, URLError
from urllib.parse import quote
from urllib.request import HTTPRedirectHandler, Request, build_opener

from . import PIPELINE_VERSION, __version__
from .config import Config
from .errors import ApiFailure, InvalidJob, LeaseLost
from .models import Job, OutputFile, PutGrant


MAX_RESPONSE_BYTES = 2 * 1024**2


def _no_duplicate_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("duplicate JSON member")
        result[key] = value
    return result


class NoRedirects(HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):  # noqa: ANN001
        return None


class ControlPlane:
    def __init__(
        self,
        config: Config,
        *,
        sleep: Callable[[float], None] = time.sleep,
        attempts: int = 4,
    ) -> None:
        self.config = config
        self._sleep = sleep
        self._attempts = attempts
        self._opener = build_opener(NoRedirects())

    def _url(self, path: str) -> str:
        return f"{self.config.api_url.rstrip('/')}{path}"

    def _request(
        self,
        method: str,
        path: str,
        payload: dict[str, Any] | None,
        *,
        allow_no_content: bool = False,
    ) -> Any | None:
        body = (
            None
            if payload is None
            else json.dumps(
                payload, ensure_ascii=False, allow_nan=False, separators=(",", ":")
            ).encode("utf-8")
        )
        for attempt in range(self._attempts):
            request = Request(
                self._url(path),
                data=body,
                method=method,
                headers={
                    "Authorization": f"Bearer {self.config.token}",
                    "Accept": "application/json",
                    "Content-Type": "application/json",
                    "User-Agent": f"scaling-neuro-processor/{__version__}",
                },
            )
            try:
                with self._opener.open(
                    request, timeout=self.config.request_timeout_seconds
                ) as response:
                    if response.status == 204 and allow_no_content:
                        return None
                    raw = response.read(MAX_RESPONSE_BYTES + 1)
                    if len(raw) > MAX_RESPONSE_BYTES:
                        raise InvalidJob()
                    if not raw:
                        return {}
                    try:
                        return json.loads(
                            raw.decode("utf-8"),
                            object_pairs_hook=_no_duplicate_object,
                            parse_constant=lambda _: (_ for _ in ()).throw(
                                ValueError()
                            ),
                        )
                    except (UnicodeDecodeError, ValueError) as exc:
                        raise InvalidJob() from exc
            except HTTPError as exc:
                if exc.code == 409:
                    raise LeaseLost() from exc
                if (
                    exc.code in {408, 425, 429, 500, 502, 503, 504}
                    and attempt + 1 < self._attempts
                ):
                    self._backoff(attempt)
                    continue
                if 400 <= exc.code < 500:
                    raise InvalidJob() from exc
                raise ApiFailure() from exc
            except (URLError, TimeoutError, OSError) as exc:
                if attempt + 1 < self._attempts:
                    self._backoff(attempt)
                    continue
                raise ApiFailure() from exc
        raise ApiFailure()

    def _backoff(self, attempt: int) -> None:
        self._sleep(min(10.0, (2**attempt) + random.random()))

    def claim(self) -> Job | None:
        payload: dict[str, Any] = {
            "processor_id": self.config.processor_id,
            "lease_seconds": self.config.lease_seconds,
        }
        if self.config.claim_input_format is not None:
            payload["claim_input_format"] = self.config.claim_input_format
        if self.config.controller_source_sha256 is not None:
            payload.update(
                {
                    "processor_version": __version__,
                    "pipeline_version": PIPELINE_VERSION,
                    "controller_source_sha256": self.config.controller_source_sha256,
                }
            )
        value = self._request(
            "POST",
            "/v1/processor/jobs/claim",
            payload,
            allow_no_content=True,
        )
        return None if value is None else Job.from_json(value)

    def heartbeat(self, job: Job) -> None:
        self._request(
            "POST",
            f"/v1/processor/jobs/{quote(job.job_id, safe='')}/heartbeat",
            {
                "lease_token": job.lease_token,
                "lease_seconds": self.config.lease_seconds,
            },
        )

    def output_grants(self, job: Job, outputs: list[OutputFile]) -> dict[str, PutGrant]:
        value = self._request(
            "POST",
            f"/v1/processor/jobs/{quote(job.job_id, safe='')}/outputs",
            {
                "lease_token": job.lease_token,
                "outputs": [output.descriptor() for output in outputs],
            },
        )
        if not isinstance(value, dict) or not isinstance(value.get("outputs"), list):
            raise ApiFailure("OUTPUT_GRANT_INVALID")
        try:
            grants = [PutGrant.from_json(item) for item in value["outputs"]]
        except InvalidJob as exc:
            raise ApiFailure("OUTPUT_GRANT_INVALID") from exc
        result = {grant.kind: grant for grant in grants}
        expected = {output.kind for output in outputs}
        if len(result) != len(grants) or set(result) != expected:
            raise ApiFailure("OUTPUT_GRANT_INVALID")
        return result

    def complete(
        self,
        job: Job,
        outputs: list[OutputFile],
        validation: dict[str, Any],
        *,
        dcm2niix_version: str | None = None,
    ) -> None:
        payload: dict[str, Any] = {
            "lease_token": job.lease_token,
            "processor_version": __version__,
            "outputs": [output.descriptor() for output in outputs],
            "validation": validation,
        }
        if dcm2niix_version is not None:
            payload["dcm2niix_version"] = dcm2niix_version
        self._request(
            "POST",
            f"/v1/processor/jobs/{quote(job.job_id, safe='')}/complete",
            payload,
        )

    def fail(
        self, job: Job, *, retryable: bool, error_code: str
    ) -> Literal["queued", "failed"]:
        value = self._request(
            "POST",
            f"/v1/processor/jobs/{quote(job.job_id, safe='')}/fail",
            {
                "lease_token": job.lease_token,
                "retryable": retryable,
                "error_code": error_code[:64],
                "error_message": error_code[:128],
            },
        )
        if (
            not isinstance(value, dict)
            or value.get("job_id") != job.job_id
            or value.get("status") not in {"queued", "failed"}
        ):
            raise ApiFailure("FAIL_RESPONSE_INVALID")
        return value["status"]
