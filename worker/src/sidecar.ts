import Ajv2020 from "ajv/dist/2020";
import commonSchema from "../../schemas/common-v1.schema.json";
import scanSidecarSchema from "../../schemas/scan-sidecar-v1.schema.json";
import { AppError } from "./errors";
import type { SidecarImageFacts } from "./nifti";

export interface SidecarExpectation {
  bundle_id: string;
  series_id: string;
  subject_id: string;
  session_id: string;
  protocol_group_id: string;
  client_version: string;
  nii_relative_key: string;
  nii_size: number;
  nii_sha256: string;
  nii_uncompressed_sha256: string;
}

export interface ValidatedSidecar {
  image: SidecarImageFacts;
}

const ajv = new Ajv2020({
  allErrors: true,
  strict: true,
  validateFormats: false,
});
ajv.addSchema(commonSchema);
const validateScanSidecar = ajv.compile(scanSidecarSchema);
const decoder = new TextDecoder("utf-8", { fatal: true });

function mismatch(message: string): never {
  throw new AppError("OBJECT_MISMATCH", 409, message);
}

/**
 * Validate the privacy boundary again at the control plane. The desktop client
 * is responsible for producing the minimized sidecar, but catalog commit does
 * not trust arbitrary JSON bytes from an enrolled device.
 */
export function validateSidecarBytes(
  bytes: Uint8Array,
  expected: SidecarExpectation,
): ValidatedSidecar {
  let value: unknown;
  try {
    value = JSON.parse(decoder.decode(bytes)) as unknown;
  } catch {
    mismatch("Metadata object is not valid UTF-8 JSON");
  }
  if (!validateScanSidecar(value)) {
    mismatch("Metadata object does not satisfy the scan-sidecar contract");
  }

  const sidecar = value as {
    bundle_id: string;
    series_id: string;
    subject_id: string;
    session_id: string;
    protocol_group_id: string;
    conversion: { client_version: string };
    image: SidecarImageFacts;
    files: {
      nifti: {
        filename: string;
        size_bytes: number;
        sha256: string;
        uncompressed_sha256: string;
      };
    };
  };
  const filename = expected.nii_relative_key.split("/").at(-1);
  if (
    !filename ||
    sidecar.bundle_id !== expected.bundle_id ||
    sidecar.series_id !== expected.series_id ||
    sidecar.subject_id !== expected.subject_id ||
    sidecar.session_id !== expected.session_id ||
    sidecar.protocol_group_id !== expected.protocol_group_id ||
    sidecar.conversion.client_version !== expected.client_version ||
    sidecar.files.nifti.filename !== filename ||
    sidecar.files.nifti.size_bytes !== expected.nii_size ||
    sidecar.files.nifti.sha256 !== expected.nii_sha256 ||
    sidecar.files.nifti.uncompressed_sha256 !==
      expected.nii_uncompressed_sha256
  ) {
    mismatch("Metadata object does not match its allocated scan bundle");
  }
  return { image: sidecar.image };
}
