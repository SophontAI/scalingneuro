#!/usr/bin/env python3
"""Validate Scaling Neuro schemas, examples, and policy/schema consistency."""

from __future__ import annotations

import hashlib
import json
import re
import sys
from pathlib import Path
from typing import Any

from jsonschema import Draft202012Validator, FormatChecker
from referencing import Registry, Resource


ROOT = Path(__file__).resolve().parent

EXAMPLE_SCHEMAS = {
    "enrollment-request-v1.example.json": "enrollment-request-v1.schema.json",
    "enrollment-response-v1.example.json": "enrollment-response-v1.schema.json",
    "local-manifest-v1.example.json": "local-manifest-v1.schema.json",
    "scan-sidecar-v1.example.json": "scan-sidecar-v1.schema.json",
    "upload-init-v1.example.json": "upload-init-v1.schema.json",
    "upload-complete-v1.example.json": "upload-complete-v1.schema.json",
    "api-error-v1.example.json": "api-error-v1.schema.json",
    "upload-status-v1.example.json": "upload-status-v1.schema.json",
    "upload-session-v1.example.json": "upload-session-v1.schema.json",
    "upload-part-request-v1.example.json": "upload-part-request-v1.schema.json",
    "upload-part-response-v1.example.json": "upload-part-response-v1.schema.json",
    "archive-manifest-v1.example.json": "archive-manifest-v1.schema.json",
}


def read_json(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise ValueError(f"{path.relative_to(ROOT)}: {exc}") from exc
    if not isinstance(value, dict):
        raise ValueError(f"{path.relative_to(ROOT)}: root must be an object")
    return value


def public_schemas() -> tuple[dict[Path, dict[str, Any]], Registry[Any]]:
    schemas: dict[Path, dict[str, Any]] = {}
    resources: list[tuple[str, Resource[Any]]] = []
    for path in sorted(ROOT.glob("*-v1.schema.json")):
        schema = read_json(path)
        Draft202012Validator.check_schema(schema)
        schema_id = schema.get("$id")
        if not isinstance(schema_id, str) or not schema_id.startswith(
            "https://scalingneuro.com/schemas/"
        ):
            raise ValueError(f"{path.name}: missing canonical Scaling Neuro $id")
        schemas[path] = schema
        resources.append((schema_id, Resource.from_contents(schema)))
    if not schemas:
        raise ValueError("no public schemas found")
    return schemas, Registry().with_resources(resources)


def validate_instance(
    instance_path: Path,
    schema_path: Path,
    schemas: dict[Path, dict[str, Any]],
    registry: Registry[Any],
) -> None:
    instance = read_json(instance_path)
    validator = Draft202012Validator(
        schemas[schema_path], registry=registry, format_checker=FormatChecker()
    )
    errors = sorted(validator.iter_errors(instance), key=lambda error: list(error.path))
    if errors:
        rendered = []
        for error in errors:
            location = "/" + "/".join(str(part) for part in error.absolute_path)
            rendered.append(f"  {location}: {error.message}")
        raise ValueError(
            f"{instance_path.relative_to(ROOT)} does not match {schema_path.name}:\n"
            + "\n".join(rendered)
        )


def schema_property_at_pointer(schema: dict[str, Any], pointer: str) -> dict[str, Any] | None:
    node: dict[str, Any] = schema
    for segment in pointer.lstrip("/").split("/"):
        properties = node.get("properties")
        if not isinstance(properties, dict):
            return None
        child = properties.get(segment)
        if not isinstance(child, dict):
            return None
        node = child
    return node


def validate_metadata_policy(
    schemas: dict[Path, dict[str, Any]], registry: Registry[Any]
) -> None:
    policy_path = ROOT / "metadata-policy-v1.json"
    policy_schema_path = ROOT / "metadata-policy-v1.schema.json"
    validate_instance(policy_path, policy_schema_path, schemas, registry)

    policy = read_json(policy_path)
    sidecar_schema = schemas[ROOT / "scan-sidecar-v1.schema.json"]
    output_paths = [rule["output_path"] for rule in policy["allowed_fields"]]
    duplicates = sorted({path for path in output_paths if output_paths.count(path) > 1})
    if duplicates:
        raise ValueError(f"metadata policy contains duplicate output paths: {duplicates}")
    missing = [
        pointer
        for pointer in output_paths
        if schema_property_at_pointer(sidecar_schema, pointer) is None
    ]
    if missing:
        raise ValueError(f"metadata policy paths absent from scan sidecar schema: {missing}")

    metadata_policy = sidecar_schema["properties"]["metadata_policy"]["properties"]
    if metadata_policy["policy_id"].get("const") != policy["policy_id"]:
        raise ValueError("sidecar metadata policy ID does not match policy artifact")
    if metadata_policy["policy_version"].get("const") != policy["schema_version"]:
        raise ValueError("sidecar metadata policy version does not match policy artifact")


def rust_struct_fields(source: str, name: str) -> set[str]:
    match = re.search(rf"pub struct {re.escape(name)}\s*\{{(.*?)\n\}}", source, re.DOTALL)
    if match is None:
        raise ValueError(f"client model is missing Rust struct {name}")
    return set(re.findall(r"pub\s+([a-z][a-z0-9_]*)\s*:", match.group(1)))


def validate_client_shape(schemas: dict[Path, dict[str, Any]]) -> None:
    """When client/ is present, prevent Rust/schema field drift."""
    model_path = ROOT.parent / "client" / "src" / "model.rs"
    convert_path = ROOT.parent / "client" / "src" / "convert.rs"
    lib_path = ROOT.parent / "client" / "src" / "lib.rs"
    bundle_path = ROOT.parent / "client" / "src" / "bundle.rs"
    if not all(path.exists() for path in [model_path, convert_path, lib_path, bundle_path]):
        return

    model = model_path.read_text(encoding="utf-8")
    sidecar = schemas[ROOT / "scan-sidecar-v1.schema.json"]
    common = schemas[ROOT / "common-v1.schema.json"]
    comparisons = {
        "LocalManifest": schemas[ROOT / "local-manifest-v1.schema.json"]["properties"],
        "ManifestBundle": schemas[ROOT / "local-manifest-v1.schema.json"]["$defs"]["manifestBundle"]["properties"],
        "ManifestObject": schemas[ROOT / "local-manifest-v1.schema.json"]["$defs"]["localObject"]["properties"],
        "ScanSidecar": sidecar["properties"],
        "SourceMetadata": sidecar["properties"]["source"]["properties"],
        "ImageMetadata": sidecar["properties"]["image"]["properties"],
        "BundleFiles": sidecar["properties"]["files"]["properties"],
        "ConversionProvenance": sidecar["properties"]["conversion"]["properties"],
        "MetadataPolicy": sidecar["properties"]["metadata_policy"]["properties"],
        "Classification": common["$defs"]["classification"]["properties"],
        "ClassificationEvidence": common["$defs"]["classification"]["properties"]["evidence"]["items"]["properties"],
        "QcResult": common["$defs"]["qualityControl"]["properties"],
        "QcCheck": common["$defs"]["qualityControl"]["properties"]["checks"]["items"]["properties"],
        "FileDigest": common["$defs"]["niftiFile"]["properties"],
    }
    for struct_name, properties in comparisons.items():
        rust_fields = rust_struct_fields(model, struct_name)
        schema_fields = set(properties) - {"$schema"}
        if rust_fields != schema_fields:
            raise ValueError(
                f"{struct_name} differs from scan contract; "
                f"Rust-only={sorted(rust_fields - schema_fields)}, "
                f"schema-only={sorted(schema_fields - rust_fields)}"
            )

    converter_source = convert_path.read_text(encoding="utf-8")
    arguments_match = re.search(
        r"CONVERSION_ARGUMENTS:\s*&\[&str\]\s*=\s*&\[(.*?)\];",
        converter_source,
        re.DOTALL,
    )
    if arguments_match is None:
        raise ValueError("client converter is missing CONVERSION_ARGUMENTS")
    uncommented = re.sub(r"//.*", "", arguments_match.group(1))
    actual_arguments = re.findall(r'"([^"]*)"', uncommented)
    actual_arguments.extend(["-f", "series"])
    expected_arguments = sidecar["properties"]["conversion"]["properties"]["arguments"]["const"]
    if actual_arguments != expected_arguments:
        raise ValueError("client dcm2niix arguments differ from scan-sidecar provenance contract")

    lib_source = lib_path.read_text(encoding="utf-8")
    bundle_source = bundle_path.read_text(encoding="utf-8")

    def rust_string_constant(source: str, name: str) -> str:
        match = re.search(
            rf"(?:pub\s+)?const\s+{re.escape(name)}:\s*&str\s*=\s*\"([^\"]+)\"",
            source,
        )
        if match is None:
            raise ValueError(f"client is missing string constant {name}")
        return match.group(1)

    schema_version = common["$defs"]["schemaVersion"]["const"]
    if rust_string_constant(lib_source, "SIDECAR_SCHEMA_VERSION") != schema_version:
        raise ValueError("client sidecar schema version differs from public contract")
    local_manifest_version = schemas[ROOT / "local-manifest-v1.schema.json"]["properties"]["schema_version"]["const"]
    if rust_string_constant(lib_source, "MANIFEST_SCHEMA_VERSION") != local_manifest_version:
        raise ValueError("client local-manifest version differs from public contract")
    converter_version = sidecar["properties"]["conversion"]["properties"]["converter_version"]["const"]
    if rust_string_constant(lib_source, "PINNED_DCM2NIIX_VERSION") != converter_version:
        raise ValueError("client converter pin differs from public contract")
    policy = read_json(ROOT / "metadata-policy-v1.json")
    if rust_string_constant(bundle_source, "METADATA_POLICY_ID") != policy["policy_id"]:
        raise ValueError("client metadata policy ID differs from policy artifact")
    if rust_string_constant(bundle_source, "METADATA_POLICY_VERSION") != policy["schema_version"]:
        raise ValueError("client metadata policy version differs from policy artifact")


def canonical_json(value: Any) -> bytes:
    return json.dumps(
        value,
        ensure_ascii=False,
        separators=(",", ":"),
        sort_keys=True,
    ).encode("utf-8")


def validate_example_consistency() -> None:
    archive = read_json(ROOT / "examples" / "archive-manifest-v1.example.json")
    upload_init = read_json(ROOT / "examples" / "upload-init-v1.example.json")
    upload_session = read_json(ROOT / "examples" / "upload-session-v1.example.json")
    part_request = read_json(ROOT / "examples" / "upload-part-request-v1.example.json")
    part_response = read_json(ROOT / "examples" / "upload-part-response-v1.example.json")
    upload_complete = read_json(ROOT / "examples" / "upload-complete-v1.example.json")
    sidecar = read_json(ROOT / "examples" / "scan-sidecar-v1.example.json")
    if len({bundle["subject_id"] for bundle in upload_init["bundles"]}) != 1:
        raise ValueError("upload-init example must contain exactly one subject")
    if len(upload_init["bundles"]) != len(archive["bundles"]):
        raise ValueError("upload-init/archive example bundle counts differ")

    initialized_by_id = {
        bundle["bundle_id"]: bundle for bundle in upload_init["bundles"]
    }
    completion_by_key = {item["key"]: item for item in upload_complete["objects"]}
    session_by_key = {item["key"]: item for item in upload_session["multipart_objects"]}
    for bundle in archive["bundles"]:
        initialized = initialized_by_id.get(bundle["bundle_id"])
        if initialized is None:
            raise ValueError("archive example bundle is absent from upload-init example")
        for field in (
            "bundle_id",
            "series_id",
            "subject_id",
            "session_id",
            "protocol_group_id",
        ):
            if initialized[field] != bundle[field] or sidecar[field] != bundle[field]:
                raise ValueError(f"example identity field {field} is inconsistent")

        for role in ("nii", "metadata"):
            initialized_object = initialized[role]
            archived_object = bundle[role]
            expected_key = archive["archive_prefix"] + initialized_object["relative_key"]
            if archived_object["key"] != expected_key:
                raise ValueError(f"archive example {role} key differs from upload-init")
            for field in ("size", "sha256"):
                if initialized_object[field] != archived_object[field]:
                    raise ValueError(f"archive example {role} {field} differs from upload-init")
            if role == "nii" and initialized_object["uncompressed_sha256"] != archived_object["uncompressed_sha256"]:
                raise ValueError("archive example NIfTI uncompressed hash differs from upload-init")
            completed = completion_by_key.get(expected_key)
            if completed is None or any(
                completed[field] != archived_object[field] for field in ("key", "size", "sha256")
            ):
                raise ValueError(f"completion example {role} differs from archive")
            if expected_key not in session_by_key:
                raise ValueError(f"upload-session example omits initialized {role} object")

        identity = {
            "series_id": bundle["series_id"],
            "subject_id": bundle["subject_id"],
            "session_id": bundle["session_id"],
            "nii": {"uncompressed_sha256": bundle["nii"]["uncompressed_sha256"]},
        }
        expected_hash = hashlib.sha256(canonical_json(identity)).hexdigest()
        if bundle["bundle_hash"] != expected_hash:
            raise ValueError("archive example bundle_hash does not match canonical identity")

    initialized_nifti = upload_init["bundles"][0]["nii"]
    sidecar_nifti = sidecar["files"]["nifti"]
    if sidecar_nifti["filename"] != initialized_nifti["relative_key"].rsplit("/", 1)[-1]:
        raise ValueError("sidecar example NIfTI filename differs from upload-init")
    for sidecar_field, initialized_field in (
        ("size_bytes", "size"),
        ("sha256", "sha256"),
        ("uncompressed_sha256", "uncompressed_sha256"),
    ):
        if sidecar_nifti[sidecar_field] != initialized_nifti[initialized_field]:
            raise ValueError(
                f"sidecar example NIfTI {sidecar_field} differs from upload-init"
            )

    if upload_session["upload_id"] != archive["upload_id"]:
        raise ValueError("upload-session/archive example upload IDs differ")
    if upload_session["object_prefix"] != archive["archive_prefix"]:
        raise ValueError("upload-session/archive example prefixes differ")
    requested_plan = session_by_key.get(part_request["key"])
    if requested_plan is None:
        raise ValueError("upload-part request key is absent from multipart plan")
    archived_size = completion_by_key[part_request["key"]]["size"]
    expected_part_size = min(requested_plan["part_size"], archived_size)
    if part_request["part_number"] != 1 or part_request["size"] != expected_part_size:
        raise ValueError("upload-part request example does not match allocated first part")
    if part_response["headers"]["content-length"] != str(part_request["size"]):
        raise ValueError("upload-part response content-length differs from request")
    if part_response["headers"]["x-amz-content-sha256"] != part_request["sha256"]:
        raise ValueError("upload-part response payload hash differs from request")

    status = read_json(ROOT / "examples" / "upload-status-v1.example.json")
    stored_manifest_bytes = canonical_json(archive) + b"\n"
    expected_manifest_hash = hashlib.sha256(stored_manifest_bytes).hexdigest()
    if status["manifest"]["sha256"] != expected_manifest_hash:
        raise ValueError("status example manifest hash does not match canonical archive example")

    first = archive["bundles"][0]
    expected_total = sum(
        bundle[role]["size"]
        for bundle in archive["bundles"]
        for role in ("nii", "metadata")
    )
    if status["total_bytes"] != expected_total:
        raise ValueError("status example total_bytes differs from archive example")
    if status["upload_id"] != archive["upload_id"]:
        raise ValueError("status/archive example upload IDs differ")
    expected_manifest_key = (
        f"manifests/v1/{archive['site_id']}/{archive['project_id']}/"
        f"{archive['upload_id']}.json"
    )
    if status["manifest"]["key"] != expected_manifest_key:
        raise ValueError("status example manifest key differs from archive identity")
    if not first["nii"]["key"].startswith(archive["archive_prefix"]):
        raise ValueError("archive example NIfTI key is outside archive_prefix")


def validate_api_error_detail_variants(
    schemas: dict[Path, dict[str, Any]], registry: Registry[Any]
) -> None:
    """Exercise every deliberately public, privacy-safe error-detail shape."""
    schema = schemas[ROOT / "api-error-v1.schema.json"]
    validator = Draft202012Validator(
        schema, registry=registry, format_checker=FormatChecker()
    )
    variants = [
        {"field": "sha256"},
        {"upload_id": "0190f86f-e0de-7f2a-a24c-0a6abf16ec81"},
        {
            "bundle_id": "7c2a5f77f3ab6c6d9e011234",
            "series_id": "45fa0d3f9a2e9af111223344",
        },
        {
            "key": "archive/v1/0190f870-1111-7f2a-a24c-0a6abf16ec81/"
            "0190f870-2222-7f2a-a24c-0a6abf16ec81/"
            "0190f86f-e0de-7f2a-a24c-0a6abf16ec81/"
            "7c2a5f77f3ab6c6d9e011234/scan.json"
        },
        {"consent_policy_version": "pilot-2026-07"},
    ]
    for details in variants:
        instance = {
            "error": {
                "code": "CONFLICT",
                "message": "Safe operational message",
                "request_id": "req_0190f86fe0de7f2aa24c0a6abf16ec81",
                "details": details,
            }
        }
        errors = list(validator.iter_errors(instance))
        if errors:
            raise ValueError(
                f"API error detail variant {sorted(details)} is invalid: "
                + "; ".join(error.message for error in errors)
            )


def main() -> int:
    try:
        schemas, registry = public_schemas()
        for example_name, schema_name in EXAMPLE_SCHEMAS.items():
            validate_instance(
                ROOT / "examples" / example_name,
                ROOT / schema_name,
                schemas,
                registry,
            )
        validate_metadata_policy(schemas, registry)
        validate_client_shape(schemas)
        validate_example_consistency()
        validate_api_error_detail_variants(schemas, registry)
    except (KeyError, TypeError, ValueError) as exc:
        print(f"schema validation failed: {exc}", file=sys.stderr)
        return 1

    print(
        f"validated {len(schemas)} schemas, {len(EXAMPLE_SCHEMAS)} examples, "
        "the metadata policy, and any present client contract"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
