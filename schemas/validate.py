#!/usr/bin/env python3
"""Validate the minimal functional EPI schemas and examples."""

from __future__ import annotations

import copy
import json
from pathlib import Path
from typing import Any

from jsonschema import Draft202012Validator, FormatChecker
from referencing import Registry, Resource


ROOT = Path(__file__).resolve().parent
EXAMPLE_SCHEMAS = {
    "api-error-v1.example.json": "api-error-v1.schema.json",
    "archive-access-request-v1.example.json": "archive-access-request-v1.schema.json",
    "archive-access-request-v2.example.json": "archive-access-request-v2.schema.json",
    "archive-access-request-v3.example.json": "archive-access-request-v3.schema.json",
    "archive-access-response-v1.example.json": "archive-access-response-v1.schema.json",
    "archive-list-v1.example.json": "archive-list-v1.schema.json",
    "contribution-info-v1.example.json": "contribution-info-v1.schema.json",
    "device-policy-v1.example.json": "device-policy-v1.schema.json",
    "dicom-archive-manifest-v2.example.json": "dicom-archive-manifest-v2.schema.json",
    "dicom-upload-init-v1.example.json": "dicom-upload-init-v1.schema.json",
    "dicom-upload-session-checkpointed-v1.example.json": "dicom-upload-session-v1.schema.json",
    "dicom-upload-session-v1.example.json": "dicom-upload-session-v1.schema.json",
    "dicom-upload-status-already-received-v1.example.json": "dicom-upload-status-v1.schema.json",
    "dicom-upload-status-v1.example.json": "dicom-upload-status-v1.schema.json",
    "registration-response-v1.example.json": "registration-response-v1.schema.json",
    "local-manifest-v1.example.json": "local-manifest-v1.schema.json",
    "registration-request-v1.example.json": "registration-request-v1.schema.json",
    "upload-complete-v1.example.json": "upload-complete-v1.schema.json",
    "upload-part-request-v1.example.json": "upload-part-request-v1.schema.json",
    "upload-part-response-v1.example.json": "upload-part-response-v1.schema.json",
}


def read_json(path: Path) -> dict[str, Any]:
    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict):
        raise ValueError(f"{path.name}: root must be an object")
    return value


def load_schemas() -> tuple[dict[str, dict[str, Any]], Registry[Any]]:
    schemas: dict[str, dict[str, Any]] = {}
    resources: list[tuple[str, Resource[Any]]] = []
    for path in sorted(ROOT.glob("*-v*.schema.json")):
        schema = read_json(path)
        Draft202012Validator.check_schema(schema)
        schema_id = schema.get("$id")
        if not isinstance(schema_id, str) or not schema_id.startswith(
            "https://scalingneuro.com/schemas/"
        ):
            raise ValueError(f"{path.name}: canonical $id is missing")
        schemas[path.name] = schema
        resources.append((schema_id, Resource.from_contents(schema)))
    return schemas, Registry().with_resources(resources)


def validator(
    schema_name: str,
    schemas: dict[str, dict[str, Any]],
    registry: Registry[Any],
) -> Draft202012Validator:
    return Draft202012Validator(
        schemas[schema_name],
        registry=registry,
        format_checker=FormatChecker(),
    )


def assert_valid(
    value: dict[str, Any],
    schema_name: str,
    schemas: dict[str, dict[str, Any]],
    registry: Registry[Any],
    label: str,
) -> None:
    errors = sorted(
        validator(schema_name, schemas, registry).iter_errors(value),
        key=lambda error: list(error.path),
    )
    if errors:
        detail = "; ".join(error.message for error in errors)
        raise ValueError(f"{label} does not match {schema_name}: {detail}")


def assert_invalid(
    value: dict[str, Any],
    schema_name: str,
    schemas: dict[str, dict[str, Any]],
    registry: Registry[Any],
    label: str,
) -> None:
    if not list(validator(schema_name, schemas, registry).iter_errors(value)):
        raise ValueError(f"{label} unexpectedly matches {schema_name}")


def main() -> None:
    schemas, registry = load_schemas()
    example_names = {
        path.name for path in (ROOT / "examples").glob("*.example.json")
    }
    if example_names != set(EXAMPLE_SCHEMAS):
        raise ValueError(
            "Example/schema map drift: "
            f"unmapped={sorted(example_names - set(EXAMPLE_SCHEMAS))}, "
            f"missing={sorted(set(EXAMPLE_SCHEMAS) - example_names)}"
        )

    for example_name, schema_name in EXAMPLE_SCHEMAS.items():
        assert_valid(
            read_json(ROOT / "examples" / example_name),
            schema_name,
            schemas,
            registry,
            example_name,
        )

    upload = read_json(
        ROOT / "examples" / "dicom-upload-init-v1.example.json"
    )
    boundary = copy.deepcopy(upload)
    boundary["series"][0]["dicom_count"] = 500_000
    assert_valid(
        boundary,
        "dicom-upload-init-v1.schema.json",
        schemas,
        registry,
        "500000-instance boundary",
    )
    boundary["series"][0]["dicom_count"] = 500_001
    assert_invalid(
        boundary,
        "dicom-upload-init-v1.schema.json",
        schemas,
        registry,
        "500001-instance boundary",
    )

    structural = copy.deepcopy(upload)
    structural["series"][0]["series_kind"] = "structural_t1w"
    assert_invalid(
        structural,
        "dicom-upload-init-v1.schema.json",
        schemas,
        registry,
        "structural series",
    )
    multiple = copy.deepcopy(upload)
    multiple["series"].append(copy.deepcopy(multiple["series"][0]))
    assert_invalid(
        multiple,
        "dicom-upload-init-v1.schema.json",
        schemas,
        registry,
        "multi-series receipt",
    )

    status = read_json(
        ROOT / "examples" / "dicom-upload-status-v1.example.json"
    )
    status["processing"] = {"status": "unexpected"}
    assert_invalid(
        status,
        "dicom-upload-status-v1.schema.json",
        schemas,
        registry,
        "post-upload processing field",
    )

    access = read_json(
        ROOT / "examples" / "archive-access-request-v3.example.json"
    )
    access["contributor_attestation"] = False
    assert_invalid(
        access,
        "archive-access-request-v3.schema.json",
        schemas,
        registry,
        "contributor without attestation",
    )
    noncontributor = read_json(
        ROOT / "examples" / "archive-access-request-v3.example.json"
    )
    noncontributor["plans_to_contribute"] = False
    noncontributor["contributor_attestation"] = False
    noncontributor["accepted_contribution_policy_version"] = None
    assert_valid(
        noncontributor,
        "archive-access-request-v3.schema.json",
        schemas,
        registry,
        "noncontributing access form",
    )
    missing_contribution_plan = copy.deepcopy(noncontributor)
    del missing_contribution_plan["plans_to_contribute"]
    assert_invalid(
        missing_contribution_plan,
        "archive-access-request-v3.schema.json",
        schemas,
        registry,
        "access form without contribution plan",
    )
    stale_contribution_policy = read_json(
        ROOT / "examples" / "archive-access-request-v3.example.json"
    )
    stale_contribution_policy["accepted_contribution_policy_version"] = (
        "open-epi-2.0.0"
    )
    assert_invalid(
        stale_contribution_policy,
        "archive-access-request-v3.schema.json",
        schemas,
        registry,
        "stale contribution policy",
    )
    stale_access_policy = read_json(
        ROOT / "examples" / "archive-access-request-v3.example.json"
    )
    stale_access_policy["accepted_data_use_policy_version"] = "archive-access-0.9.0"
    assert_invalid(
        stale_access_policy,
        "archive-access-request-v3.schema.json",
        schemas,
        registry,
        "stale archive access and privacy agreement",
    )

    print(
        f"Validated {len(schemas)} minimal EPI schemas and "
        f"{len(EXAMPLE_SCHEMAS)} examples."
    )


if __name__ == "__main__":
    main()
