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
  formats: { "date-time": true, uri: true, uuid: true },
});

const readJson = (file) => JSON.parse(fs.readFileSync(file, "utf8"));
const schemaFiles = fs
  .readdirSync(root)
  .filter((name) => name.endsWith("-v1.schema.json"))
  .sort();

for (const name of schemaFiles) {
  ajv.addSchema(readJson(path.join(root, name)));
}

const exampleSchemas = {
  "api-error-v1.example.json": "api-error-v1.schema.json",
  "archive-manifest-v1.example.json": "archive-manifest-v1.schema.json",
  "enrollment-request-v1.example.json": "enrollment-request-v1.schema.json",
  "enrollment-response-v1.example.json": "enrollment-response-v1.schema.json",
  "local-manifest-v1.example.json": "local-manifest-v1.schema.json",
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

console.log(
  `strict-Ajv validated ${schemaFiles.length} schemas, ${Object.keys(exampleSchemas).length} examples, and the metadata policy`,
);
