#!/usr/bin/env node

import fs from "node:fs";
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const root = path.dirname(fileURLToPath(import.meta.url));
const requireFromWorker = createRequire(new URL("../worker/package.json", import.meta.url));
const Ajv2020 = requireFromWorker("ajv/dist/2020").default;

const ajv = new Ajv2020({
  allErrors: true,
  strict: true,
  formats: { "date-time": true, email: true, uri: true, uuid: true },
});

const readJson = (file) => JSON.parse(fs.readFileSync(file, "utf8"));
const schemaFiles = fs
  .readdirSync(root)
  .filter((name) => /-v[0-9]+\.schema\.json$/u.test(name))
  .sort();

for (const name of schemaFiles) {
  ajv.addSchema(readJson(path.join(root, name)));
}

const exampleSchemas = {
  "api-error-v1.example.json": "api-error-v1.schema.json",
  "archive-manifest-v1.example.json": "archive-manifest-v1.schema.json",
  "contribution-info-v1.example.json": "contribution-info-v1.schema.json",
  "dicom-upload-init-v1.example.json": "dicom-upload-init-v1.schema.json",
  "dicom-upload-session-checkpointed-v1.example.json": "dicom-upload-session-v1.schema.json",
  "dicom-upload-session-v1.example.json": "dicom-upload-session-v1.schema.json",
  "dicom-upload-status-already-received-v1.example.json": "dicom-upload-status-v1.schema.json",
  "dicom-upload-status-v1.example.json": "dicom-upload-status-v1.schema.json",
  "device-policy-v1.example.json": "device-policy-v1.schema.json",
  "dicom-archive-manifest-v2.example.json": "dicom-archive-manifest-v2.schema.json",
  "enrollment-request-v1.example.json": "enrollment-request-v1.schema.json",
  "enrollment-response-v1.example.json": "enrollment-response-v1.schema.json",
  "local-manifest-v1.example.json": "local-manifest-v1.schema.json",
  "registration-request-v1.example.json": "registration-request-v1.schema.json",
  "scan-sidecar-v1.example.json": "scan-sidecar-v1.schema.json",
  "upload-complete-v1.example.json": "upload-complete-v1.schema.json",
  "upload-init-v1.example.json": "upload-init-v1.schema.json",
  "upload-part-request-v1.example.json": "upload-part-request-v1.schema.json",
  "upload-part-response-v1.example.json": "upload-part-response-v1.schema.json",
  "upload-session-v1.example.json": "upload-session-v1.schema.json",
  "upload-status-v1.example.json": "upload-status-v1.schema.json",
};

for (const [example, schema] of Object.entries(exampleSchemas)) {
  const schemaId = `https://scalingneuro.com/schemas/${schema}`;
  const validate = ajv.getSchema(schemaId);
  if (!validate) throw new Error(`Ajv did not register ${schemaId}`);
  const value = readJson(path.join(root, "examples", example));
  if (!validate(value)) {
    throw new Error(`${example} failed strict Ajv validation: ${ajv.errorsText(validate.errors)}`);
  }
}

const policyValidator = ajv.getSchema(
  "https://scalingneuro.com/schemas/metadata-policy-v1.schema.json",
);
if (!policyValidator(readJson(path.join(root, "metadata-policy-v1.json")))) {
  throw new Error(`metadata policy failed strict Ajv validation: ${ajv.errorsText(policyValidator.errors)}`);
}

const dicomInitValidator = ajv.getSchema(
  "https://scalingneuro.com/schemas/dicom-upload-init-v1.schema.json",
);
const dicomInitBoundary = readJson(
  path.join(root, "examples", "dicom-upload-init-v1.example.json"),
);
dicomInitBoundary.series[0].dicom_count = 500_000;
if (!dicomInitValidator(dicomInitBoundary)) {
  throw new Error(
    `raw DICOM schema rejects 500000 instances: ${ajv.errorsText(dicomInitValidator.errors)}`,
  );
}
dicomInitBoundary.series[0].dicom_count = 500_001;
if (dicomInitValidator(dicomInitBoundary)) {
  throw new Error("raw DICOM schema accepts more than 500000 instances");
}

const localManifestValidator = ajv.getSchema(
  "https://scalingneuro.com/schemas/local-manifest-v1.schema.json",
);
const localManifestSchema = readJson(path.join(root, "local-manifest-v1.schema.json"));
if (Object.hasOwn(localManifestSchema.properties.bundles, "maxItems")) {
  throw new Error("local manifest imposes a false whole-folder series limit");
}
const localManifestBoundary = readJson(
  path.join(root, "examples", "local-manifest-v1.example.json"),
);
const rawBundle = localManifestBoundary.bundles.find((bundle) => bundle.archive);
if (!rawBundle) throw new Error("local manifest example has no raw DICOM bundle");
rawBundle.archive.dicom_instance_count = 500_000;
rawBundle.source_dicom_count = 500_000;
if (!localManifestValidator(localManifestBoundary)) {
  throw new Error(
    `local manifest schema rejects 500000 instances: ${ajv.errorsText(localManifestValidator.errors)}`,
  );
}
for (const [parent, field] of [
  ["archive", "dicom_instance_count"],
  [null, "source_dicom_count"],
]) {
  const invalid = structuredClone(localManifestBoundary);
  const invalidBundle = invalid.bundles.find((bundle) => bundle.archive);
  if (parent) invalidBundle[parent][field] = 500_001;
  else invalidBundle[field] = 500_001;
  if (localManifestValidator(invalid)) {
    throw new Error(`local manifest schema accepts more than 500000 at ${field}`);
  }
}

const dicomSessionValidator = ajv.getSchema(
  "https://scalingneuro.com/schemas/dicom-upload-session-v1.schema.json",
);
const checkpointedSession = readJson(
  path.join(root, "examples", "dicom-upload-session-checkpointed-v1.example.json"),
);
const checkpointWithoutPrefix = structuredClone(checkpointedSession);
delete checkpointWithoutPrefix.object_prefix;
if (dicomSessionValidator(checkpointWithoutPrefix)) {
  throw new Error("checkpointed DICOM session accepts a response without object_prefix");
}
const checkpointWithoutObjects = structuredClone(checkpointedSession);
delete checkpointWithoutObjects.multipart_objects;
if (dicomSessionValidator(checkpointWithoutObjects)) {
  throw new Error("checkpointed DICOM session accepts a response without multipart_objects");
}

const dicomStatusValidator = ajv.getSchema(
  "https://scalingneuro.com/schemas/dicom-upload-status-v1.schema.json",
);
const alreadyReceivedStatus = readJson(
  path.join(root, "examples", "dicom-upload-status-already-received-v1.example.json"),
);
const incompleteAlreadyReceived = structuredClone(alreadyReceivedStatus);
delete incompleteAlreadyReceived.already_received_series;
if (dicomStatusValidator(incompleteAlreadyReceived)) {
  throw new Error("already_received DICOM status accepts a response without reconciliation lineage");
}
const ambiguousAlreadyReceived = structuredClone(alreadyReceivedStatus);
ambiguousAlreadyReceived.object_prefix = "dicom/v1/site/project/ambiguous/";
if (dicomStatusValidator(ambiguousAlreadyReceived)) {
  throw new Error("already_received DICOM status accepts provisional object allocation fields");
}

console.log(
  `strict-Ajv validated ${schemaFiles.length} schemas, ${Object.keys(exampleSchemas).length} examples, and the metadata policy`,
);
