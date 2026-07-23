#!/usr/bin/env node

import fs from "node:fs";
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const root = path.dirname(fileURLToPath(import.meta.url));
const requireFromWorker = createRequire(
  new URL("../worker/package.json", import.meta.url),
);
const Ajv2020 = requireFromWorker("ajv/dist/2020").default;
const ajv = new Ajv2020({
  allErrors: true,
  strict: true,
  formats: { "date-time": true, email: true, uri: true },
});
const readJson = (file) => JSON.parse(fs.readFileSync(file, "utf8"));
const schemaFiles = fs
  .readdirSync(root)
  .filter((name) => /-v[0-9]+\.schema\.json$/u.test(name))
  .sort();
for (const name of schemaFiles) {
  ajv.addSchema(readJson(path.join(root, name)));
}

const pairs = {
  "api-error-v1.example.json": "api-error-v1.schema.json",
  "archive-access-request-v1.example.json":
    "archive-access-request-v1.schema.json",
  "archive-access-response-v1.example.json":
    "archive-access-response-v1.schema.json",
  "archive-list-v1.example.json": "archive-list-v1.schema.json",
  "contribution-info-v1.example.json": "contribution-info-v1.schema.json",
  "device-policy-v1.example.json": "device-policy-v1.schema.json",
  "dicom-archive-manifest-v2.example.json":
    "dicom-archive-manifest-v2.schema.json",
  "dicom-upload-init-v1.example.json": "dicom-upload-init-v1.schema.json",
  "dicom-upload-session-checkpointed-v1.example.json":
    "dicom-upload-session-v1.schema.json",
  "dicom-upload-session-v1.example.json":
    "dicom-upload-session-v1.schema.json",
  "dicom-upload-status-already-received-v1.example.json":
    "dicom-upload-status-v1.schema.json",
  "dicom-upload-status-v1.example.json":
    "dicom-upload-status-v1.schema.json",
  "registration-response-v1.example.json":
    "registration-response-v1.schema.json",
  "local-manifest-v1.example.json": "local-manifest-v1.schema.json",
  "registration-request-v1.example.json":
    "registration-request-v1.schema.json",
  "upload-complete-v1.example.json": "upload-complete-v1.schema.json",
  "upload-part-request-v1.example.json":
    "upload-part-request-v1.schema.json",
  "upload-part-response-v1.example.json":
    "upload-part-response-v1.schema.json",
};

const actualExamples = fs
  .readdirSync(path.join(root, "examples"))
  .filter((name) => name.endsWith(".example.json"))
  .sort();
const mappedExamples = Object.keys(pairs).sort();
if (JSON.stringify(actualExamples) !== JSON.stringify(mappedExamples)) {
  throw new Error("Example/schema map is incomplete");
}

const validate = (example, schema) => {
  const schemaId = `https://scalingneuro.com/schemas/${schema}`;
  const check = ajv.getSchema(schemaId);
  if (!check) throw new Error(`Ajv did not register ${schemaId}`);
  const value = readJson(path.join(root, "examples", example));
  if (!check(value)) {
    throw new Error(
      `${example} failed strict Ajv validation: ${ajv.errorsText(check.errors)}`,
    );
  }
};
for (const [example, schema] of Object.entries(pairs)) validate(example, schema);

const uploadCheck = ajv.getSchema(
  "https://scalingneuro.com/schemas/dicom-upload-init-v1.schema.json",
);
const upload = readJson(
  path.join(root, "examples", "dicom-upload-init-v1.example.json"),
);
upload.series[0].dicom_count = 500_000;
if (!uploadCheck(upload)) throw new Error("500000-instance EPI was rejected");
upload.series[0].dicom_count = 500_001;
if (uploadCheck(upload)) throw new Error("500001-instance EPI was accepted");

const structural = readJson(
  path.join(root, "examples", "dicom-upload-init-v1.example.json"),
);
structural.series[0].series_kind = "structural_t1w";
if (uploadCheck(structural)) throw new Error("Structural DICOM was accepted");

console.log(
  `Strict Ajv validated ${schemaFiles.length} minimal EPI schemas and ` +
    `${mappedExamples.length} examples.`,
);
