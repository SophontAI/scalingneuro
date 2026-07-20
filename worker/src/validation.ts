import { invalid } from "./errors";

const PSEUDONYM_ID = /^[a-f0-9]{24}$/u;
const UUID =
  /^[a-f0-9]{8}-[a-f0-9]{4}-[1-8][a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/u;
const SLUG = /^[a-z0-9][a-z0-9-]{0,62}$/u;
const VERSION = /^[A-Za-z0-9][A-Za-z0-9.+_-]{0,63}$/u;
const CLIENT_VERSION =
  /^(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)(?:-[A-Za-z0-9.-]+)?(?:\+[A-Za-z0-9.-]+)?$/u;
const SHA256 = /^[a-f0-9]{64}$/u;
const ETAG = /^[A-Za-z0-9+/=_:.-]{1,256}$/u;
const PLATFORM = /^[A-Za-z0-9][A-Za-z0-9._-]{0,31}$/u;
const RELATIVE_KEY = /^[A-Za-z0-9._~/-]{1,512}$/u;
const EMAIL = /^[A-Za-z0-9.!#$%&'*+/=?^_`{|}~-]+@[A-Za-z0-9](?:[A-Za-z0-9.-]{0,251}[A-Za-z0-9])?$/u;
const ROR_ID = /^https:\/\/ror\.org\/0[a-hj-km-np-tv-z0-9]{8}$/u;
const MAX_BUNDLES = 32;
const MAX_NIFTI_BYTES = 5 * 1024 ** 3;
const MAX_METADATA_BYTES = 8 * 1024 ** 2;
const MAX_UPLOAD_BYTES = 32 * 1024 ** 3;
// A raw receipt may require HEAD + multipart completion + authoritative HEAD
// for every object. Eight keeps even an untrusted/custom client inside the
// Cloudflare Free per-invocation subrequest and D1 query ceilings.
const MAX_DICOM_SERIES = 8;
const MAX_DICOM_INSTANCES_PER_SERIES = 500_000;
const MAX_DICOM_ARCHIVE_BYTES = 64 * 1024 ** 3;
const MAX_DICOM_UPLOAD_BYTES = 250 * 1024 ** 3;
const MAX_COMPLETION_OBJECTS = Math.max(MAX_BUNDLES * 2, MAX_DICOM_SERIES);
const PROCESSOR_ID = /^[A-Za-z0-9][A-Za-z0-9._:-]{0,95}$/u;
const ERROR_CODE = /^[A-Z0-9][A-Z0-9_]{0,63}$/u;
const DICOM_SERIES_KINDS = new Set([
  "functional_epi",
  "structural_t1w",
  "structural_t2w",
  "structural_other",
  "diffusion",
  "asl_perfusion",
  "perfusion",
  "fieldmap",
  "sbref",
  "localizer",
  "derived_mr",
  "other_mr",
]);

export interface EnrollRequest {
  invite_code: string;
  enrollment_id: string;
  device_token: string;
  device_name: string;
  client_version: string;
  platform: string;
}

export interface PublicRegistrationRequest {
  registration_id: string;
  device_token: string;
  device_name: string;
  client_version: string;
  platform: string;
  contact_email: string;
  contact_name: string;
  institution_name: string;
  institution_ror_id?: string;
  lab_name: string;
  contact_opt_in: boolean;
  accepted_consent_policy_version: string;
}

export interface ObjectDescriptor {
  relative_key: string;
  size: number;
  sha256: string;
}

export interface NiftiObjectDescriptor extends ObjectDescriptor {
  uncompressed_sha256: string;
}

export interface BundleDescriptor {
  bundle_id: string;
  series_id: string;
  subject_id: string;
  session_id: string;
  protocol_group_id: string;
  nii: NiftiObjectDescriptor;
  metadata: ObjectDescriptor;
}

export interface CreateUploadRequest {
  bundles: BundleDescriptor[];
  client_version: string;
}

export interface DicomArchiveDescriptor extends ObjectDescriptor {
  format: "dicom-tar-zstd";
}

export interface DicomSeriesDescriptor {
  series_archive_id: string;
  series_id: string;
  subject_id: string;
  session_id: string;
  protocol_group_id: string;
  dicom_count: number;
  series_kind?: string;
  processing_route?: "functional-epi-v1" | "archive-verify-v1";
  pixel_data_policy?: "scanner-native-not-defaced";
  archive: DicomArchiveDescriptor;
}

export interface CreateDicomUploadRequest {
  format: "dicom-series-v1";
  client_version: string;
  deidentification: { policy_id: string; policy_version: string };
  series: DicomSeriesDescriptor[];
}

export interface CompletedObject {
  key: string;
  size: number;
  sha256: string;
  parts: CompletedPart[];
}

export interface CompletedPart {
  part_number: number;
  etag: string;
}

export interface CompleteUploadRequest {
  objects: CompletedObject[];
}

export interface SignPartRequest {
  key: string;
  part_number: number;
  size: number;
  sha256: string;
}

export interface ProcessorClaimRequest {
  processor_id: string;
  lease_seconds: number;
  claim_input_format?: "dicom-series-v1" | "nifti-v1";
  processor_version?: string;
  pipeline_version?: string;
  controller_source_sha256?: string;
}

export type ProcessorOutputKind =
  | "nifti"
  | "sidecar"
  | "processing_manifest";

export interface ProcessorOutputDescriptor {
  kind: ProcessorOutputKind;
  size_bytes: number;
  sha256: string;
  content_type: string;
  uncompressed_sha256?: string;
}

export interface ProcessorLeaseRequest {
  lease_token: string;
  lease_seconds: number;
}

export interface ProcessorOutputRequest {
  lease_token: string;
  outputs: ProcessorOutputDescriptor[];
}

export interface DicomProcessorValidation {
  archive_sha256_verified: boolean;
  dicom_count: number;
  dicom_parse_succeeded: boolean;
  dicom_privacy_audit_succeeded?: boolean;
  functional_epi_confirmed: boolean;
}

export interface PublicPolicyAcceptanceRequest {
  accepted_consent_policy_version: string;
}

export interface NiftiProcessorValidation {
  nifti_sha256_verified: boolean;
  nifti_uncompressed_sha256_verified: boolean;
  sidecar_sha256_verified: boolean;
  nifti_header_valid: boolean;
  sidecar_valid: boolean;
  nifti_sidecar_consistent: boolean;
}

export interface ProcessorCompleteRequest {
  lease_token: string;
  processor_version: string;
  dcm2niix_version?: string;
  outputs: ProcessorOutputDescriptor[];
  validation: DicomProcessorValidation | NiftiProcessorValidation;
}

export interface ProcessorFailRequest {
  lease_token: string;
  retryable: boolean;
  error_code: string;
  error_message: string;
}

export interface AdminInviteRequest {
  site_slug: string;
  site_name: string;
  project_slug: string;
  project_name: string;
  consent_policy_version: string;
  expires_in_seconds: number;
  max_uses: number;
}

function record(value: unknown, label: string): Record<string, unknown> {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    invalid(`${label} must be an object`);
  }
  return value as Record<string, unknown>;
}

function exactKeys(
  value: Record<string, unknown>,
  required: readonly string[],
  optional: readonly string[] = [],
  label = "object",
): void {
  const allowed = new Set([...required, ...optional]);
  for (const key of Object.keys(value)) {
    if (!allowed.has(key)) invalid(`${label} contains unknown field: ${key}`);
  }
  for (const key of required) {
    if (!(key in value)) invalid(`${label}.${key} is required`);
  }
}

function text(
  value: unknown,
  label: string,
  options: { min?: number; max: number; pattern?: RegExp },
): string {
  if (typeof value !== "string") invalid(`${label} must be a string`);
  const min = options.min ?? 1;
  if (value.length < min || value.length > options.max) {
    invalid(`${label} must contain ${min}-${options.max} characters`);
  }
  if (options.pattern && !options.pattern.test(value))
    invalid(`${label} has an invalid format`);
  return value;
}

function integer(
  value: unknown,
  label: string,
  min: number,
  max: number,
): number {
  if (
    !Number.isSafeInteger(value) ||
    (value as number) < min ||
    (value as number) > max
  ) {
    invalid(`${label} must be an integer between ${min} and ${max}`);
  }
  return value as number;
}

function boolean(value: unknown, label: string): boolean {
  if (typeof value !== "boolean") invalid(`${label} must be a boolean`);
  return value;
}

function pseudonymId(value: unknown, label: string): string {
  return text(value, label, { min: 24, max: 24, pattern: PSEUDONYM_ID });
}

function etag(value: unknown, label: string): string {
  const raw = text(value, label, { max: 258 });
  const normalized =
    raw.startsWith('"') && raw.endsWith('"') ? raw.slice(1, -1) : raw;
  if (!ETAG.test(normalized)) invalid(`${label} has an invalid format`);
  return normalized;
}

function humanLabel(value: unknown, label: string, max: number): string {
  const result = text(value, label, { max });
  if (result !== result.trim() || /[\p{Cc}\p{Cf}]/u.test(result)) {
    invalid(`${label} contains unsafe whitespace or control characters`);
  }
  return result;
}

export function validateRelativeKey(value: unknown, label: string): string {
  const key = text(value, label, { max: 512, pattern: RELATIVE_KEY });
  if (key.startsWith("/") || key.endsWith("/") || key.includes("\\")) {
    invalid(`${label} must be a relative object key`);
  }
  const segments = key.split("/");
  if (
    segments.some((segment) => !segment || segment === "." || segment === "..")
  ) {
    invalid(`${label} contains an unsafe path segment`);
  }
  return key;
}

function objectDescriptor(
  value: unknown,
  label: string,
  kind: "nii",
): NiftiObjectDescriptor;
function objectDescriptor(
  value: unknown,
  label: string,
  kind: "metadata",
): ObjectDescriptor;
function objectDescriptor(
  value: unknown,
  label: string,
  kind: "nii" | "metadata",
): ObjectDescriptor | NiftiObjectDescriptor {
  const input = record(value, label);
  exactKeys(
    input,
    kind === "nii"
      ? ["relative_key", "size", "sha256", "uncompressed_sha256"]
      : ["relative_key", "size", "sha256"],
    [],
    label,
  );
  const relativeKey = validateRelativeKey(
    input.relative_key,
    `${label}.relative_key`,
  );
  if (kind === "nii" && !relativeKey.endsWith(".nii.gz")) {
    invalid(`${label}.relative_key must end with .nii.gz`);
  }
  if (kind === "metadata" && !relativeKey.endsWith(".json")) {
    invalid(`${label}.relative_key must end with .json`);
  }
  const descriptor: ObjectDescriptor = {
    relative_key: relativeKey,
    size: integer(
      input.size,
      `${label}.size`,
      kind === "nii" ? 32 : 2,
      kind === "nii" ? MAX_NIFTI_BYTES : MAX_METADATA_BYTES,
    ),
    sha256: text(input.sha256, `${label}.sha256`, { max: 64, pattern: SHA256 }),
  };
  if (kind === "nii") {
    return {
      ...descriptor,
      uncompressed_sha256: text(
        input.uncompressed_sha256,
        `${label}.uncompressed_sha256`,
        {
          max: 64,
          pattern: SHA256,
        },
      ),
    };
  }
  return descriptor;
}

export function parseEnrollRequest(value: unknown): EnrollRequest {
  const input = record(value, "request");
  exactKeys(
    input,
    [
      "invite_code",
      "enrollment_id",
      "device_token",
      "device_name",
      "client_version",
      "platform",
    ],
    [],
    "request",
  );
  return {
    invite_code: text(input.invite_code, "invite_code", {
      min: 53,
      max: 53,
      pattern: /^sn_invite_[A-Za-z0-9_-]{43}$/u,
    }),
    enrollment_id: text(input.enrollment_id, "enrollment_id", {
      min: 36,
      max: 36,
      pattern: UUID,
    }),
    device_token: text(input.device_token, "device_token", {
      min: 53,
      max: 53,
      pattern: /^sn_device_[A-Za-z0-9_-]{43}$/u,
    }),
    device_name: humanLabel(input.device_name, "device_name", 96),
    client_version: text(input.client_version, "client_version", {
      max: 64,
      pattern: CLIENT_VERSION,
    }),
    platform: text(input.platform, "platform", { max: 32, pattern: PLATFORM }),
  };
}

export function parsePublicRegistrationRequest(
  value: unknown,
): PublicRegistrationRequest {
  const input = record(value, "request");
  exactKeys(
    input,
    [
      "registration_id",
      "device_token",
      "device_name",
      "client_version",
      "platform",
      "contact_email",
      "contact_name",
      "institution_name",
      "lab_name",
      "contact_opt_in",
      "accepted_consent_policy_version",
    ],
    ["institution_ror_id"],
    "request",
  );
  const contactEmail = text(input.contact_email, "contact_email", {
    max: 254,
    pattern: EMAIL,
  }).toLowerCase();
  if (contactEmail !== contactEmail.trim() || contactEmail.includes("..")) {
    invalid("contact_email has an invalid format");
  }
  const ror = input.institution_ror_id;
  return {
    registration_id: text(input.registration_id, "registration_id", {
      min: 36,
      max: 36,
      pattern: UUID,
    }),
    device_token: text(input.device_token, "device_token", {
      min: 53,
      max: 53,
      pattern: /^sn_device_[A-Za-z0-9_-]{43}$/u,
    }),
    device_name: humanLabel(input.device_name, "device_name", 96),
    client_version: text(input.client_version, "client_version", {
      max: 64,
      pattern: CLIENT_VERSION,
    }),
    platform: text(input.platform, "platform", { max: 32, pattern: PLATFORM }),
    contact_email: contactEmail,
    contact_name: humanLabel(input.contact_name, "contact_name", 96),
    institution_name: humanLabel(
      input.institution_name,
      "institution_name",
      160,
    ),
    ...(ror === undefined
      ? {}
      : {
          institution_ror_id: text(ror, "institution_ror_id", {
            min: 25,
            max: 25,
            pattern: ROR_ID,
          }),
        }),
    lab_name: humanLabel(input.lab_name, "lab_name", 160),
    contact_opt_in: boolean(input.contact_opt_in, "contact_opt_in"),
    accepted_consent_policy_version: text(
      input.accepted_consent_policy_version,
      "accepted_consent_policy_version",
      { max: 64, pattern: VERSION },
    ),
  };
}

export function parseCreateUploadRequest(value: unknown): CreateUploadRequest {
  const input = record(value, "request");
  exactKeys(input, ["bundles", "client_version"], [], "request");
  if (
    !Array.isArray(input.bundles) ||
    input.bundles.length < 1 ||
    input.bundles.length > MAX_BUNDLES
  ) {
    invalid(`bundles must contain 1-${MAX_BUNDLES} entries`);
  }

  const bundleIds = new Set<string>();
  const relativeKeys = new Set<string>();
  const bundles = input.bundles.map((rawBundle, index): BundleDescriptor => {
    const label = `bundles[${index}]`;
    const bundle = record(rawBundle, label);
    exactKeys(
      bundle,
      [
        "bundle_id",
        "series_id",
        "subject_id",
        "session_id",
        "protocol_group_id",
        "nii",
        "metadata",
      ],
      [],
      label,
    );
    const parsed: BundleDescriptor = {
      bundle_id: pseudonymId(bundle.bundle_id, `${label}.bundle_id`),
      series_id: pseudonymId(bundle.series_id, `${label}.series_id`),
      subject_id: pseudonymId(bundle.subject_id, `${label}.subject_id`),
      session_id: pseudonymId(bundle.session_id, `${label}.session_id`),
      protocol_group_id: pseudonymId(
        bundle.protocol_group_id,
        `${label}.protocol_group_id`,
      ),
      nii: objectDescriptor(bundle.nii, `${label}.nii`, "nii"),
      metadata: objectDescriptor(
        bundle.metadata,
        `${label}.metadata`,
        "metadata",
      ),
    };
    const bundlePrefix = `${parsed.bundle_id}/`;
    if (
      !parsed.nii.relative_key.startsWith(bundlePrefix) ||
      !parsed.metadata.relative_key.startsWith(bundlePrefix)
    ) {
      invalid(
        `${label} object paths must be contained in the bundle_id directory`,
      );
    }
    const niiFilename = parsed.nii.relative_key.slice(bundlePrefix.length);
    const metadataFilename = parsed.metadata.relative_key.slice(
      bundlePrefix.length,
    );
    const niiStem = niiFilename.slice(0, -".nii.gz".length);
    const metadataStem = metadataFilename.slice(0, -".json".length);
    if (
      niiFilename.includes("/") ||
      metadataFilename.includes("/") ||
      niiStem.length === 0 ||
      niiStem !== metadataStem
    ) {
      invalid(`${label} objects must be a same-basename NIfTI and JSON pair`);
    }
    if (bundleIds.has(parsed.bundle_id))
      invalid(`${label}.bundle_id must be unique`);
    bundleIds.add(parsed.bundle_id);
    for (const key of [parsed.nii.relative_key, parsed.metadata.relative_key]) {
      if (relativeKeys.has(key))
        invalid(`${label} object paths must be unique`);
      relativeKeys.add(key);
    }
    return parsed;
  });

  const totalBytes = bundles.reduce(
    (sum, bundle) => sum + bundle.nii.size + bundle.metadata.size,
    0,
  );
  if (!Number.isSafeInteger(totalBytes) || totalBytes > MAX_UPLOAD_BYTES) {
    invalid("total declared upload size exceeds the 32 GiB session limit");
  }
  if (bundles.some((bundle) => bundle.subject_id !== bundles[0]?.subject_id)) {
    invalid("all bundles in an upload session must belong to one subject_id");
  }

  return {
    bundles,
    client_version: text(input.client_version, "client_version", {
      max: 64,
      pattern: CLIENT_VERSION,
    }),
  };
}

export function parseCreateDicomUploadRequest(
  value: unknown,
): CreateDicomUploadRequest {
  const input = record(value, "request");
  exactKeys(
    input,
    ["format", "client_version", "deidentification", "series"],
    [],
    "request",
  );
  if (input.format !== "dicom-series-v1") {
    invalid("format must be dicom-series-v1");
  }
  const deidentification = record(
    input.deidentification,
    "deidentification",
  );
  exactKeys(
    deidentification,
    ["policy_id", "policy_version"],
    [],
    "deidentification",
  );
  if (
    !Array.isArray(input.series) ||
    input.series.length < 1 ||
    input.series.length > MAX_DICOM_SERIES
  ) {
    invalid(`series must contain 1-${MAX_DICOM_SERIES} entries`);
  }
  const archiveIds = new Set<string>();
  const seriesIds = new Set<string>();
  const relativeKeys = new Set<string>();
  const series = input.series.map((raw, index): DicomSeriesDescriptor => {
    const label = `series[${index}]`;
    const item = record(raw, label);
    exactKeys(
      item,
      [
        "series_archive_id",
        "series_id",
        "subject_id",
        "session_id",
        "protocol_group_id",
        "dicom_count",
        "archive",
      ],
      ["series_kind", "processing_route", "pixel_data_policy"],
      label,
    );
    const archiveInput = record(item.archive, `${label}.archive`);
    exactKeys(
      archiveInput,
      ["format", "relative_key", "size", "sha256"],
      [],
      `${label}.archive`,
    );
    if (archiveInput.format !== "dicom-tar-zstd") {
      invalid(`${label}.archive.format must be dicom-tar-zstd`);
    }
    const seriesArchiveId = pseudonymId(
      item.series_archive_id,
      `${label}.series_archive_id`,
    );
    const seriesId = pseudonymId(item.series_id, `${label}.series_id`);
    const relativeKey = validateRelativeKey(
      archiveInput.relative_key,
      `${label}.archive.relative_key`,
    );
    if (
      relativeKey !== `${seriesArchiveId}/dicom.tar.zst` ||
      archiveIds.has(seriesArchiveId) ||
      seriesIds.has(seriesId) ||
      relativeKeys.has(relativeKey)
    ) {
      invalid(`${label} has a duplicate identity or non-canonical archive path`);
    }
    archiveIds.add(seriesArchiveId);
    seriesIds.add(seriesId);
    relativeKeys.add(relativeKey);
    let processingRoute: DicomSeriesDescriptor["processing_route"];
    if (item.processing_route !== undefined) {
      if (
        item.processing_route !== "functional-epi-v1" &&
        item.processing_route !== "archive-verify-v1"
      ) {
        invalid(`${label}.processing_route is invalid`);
      }
      processingRoute = item.processing_route;
    }
    let pixelDataPolicy: DicomSeriesDescriptor["pixel_data_policy"];
    if (item.pixel_data_policy !== undefined) {
      if (item.pixel_data_policy !== "scanner-native-not-defaced") {
        invalid(
          `${label}.pixel_data_policy must be scanner-native-not-defaced`,
        );
      }
      pixelDataPolicy = item.pixel_data_policy;
    }
    const seriesKind =
      item.series_kind === undefined
        ? undefined
        : text(item.series_kind, `${label}.series_kind`, {
            max: 64,
            pattern: /^[a-z][a-z0-9_]{0,63}$/u,
          });
    if (seriesKind !== undefined && !DICOM_SERIES_KINDS.has(seriesKind)) {
      invalid(`${label}.series_kind is invalid`);
    }
    return {
      series_archive_id: seriesArchiveId,
      series_id: seriesId,
      subject_id: pseudonymId(item.subject_id, `${label}.subject_id`),
      session_id: pseudonymId(item.session_id, `${label}.session_id`),
      protocol_group_id: pseudonymId(
        item.protocol_group_id,
        `${label}.protocol_group_id`,
      ),
      dicom_count: integer(
        item.dicom_count,
        `${label}.dicom_count`,
        1,
        MAX_DICOM_INSTANCES_PER_SERIES,
      ),
      ...(seriesKind === undefined ? {} : { series_kind: seriesKind }),
      ...(processingRoute === undefined
        ? {}
        : { processing_route: processingRoute }),
      ...(pixelDataPolicy === undefined
        ? {}
        : { pixel_data_policy: pixelDataPolicy }),
      archive: {
        format: "dicom-tar-zstd",
        relative_key: relativeKey,
        size: integer(
          archiveInput.size,
          `${label}.archive.size`,
          32,
          MAX_DICOM_ARCHIVE_BYTES,
        ),
        sha256: text(archiveInput.sha256, `${label}.archive.sha256`, {
          max: 64,
          pattern: SHA256,
        }),
      },
    };
  });
  if (series.some((item) => item.subject_id !== series[0]?.subject_id)) {
    invalid("all series in an upload session must belong to one subject_id");
  }
  const totalBytes = series.reduce((sum, item) => sum + item.archive.size, 0);
  if (!Number.isSafeInteger(totalBytes) || totalBytes > MAX_DICOM_UPLOAD_BYTES) {
    invalid("total declared DICOM upload size exceeds 250 GiB");
  }
  return {
    format: "dicom-series-v1",
    client_version: text(input.client_version, "client_version", {
      max: 64,
      pattern: CLIENT_VERSION,
    }),
    deidentification: {
      policy_id: text(deidentification.policy_id, "deidentification.policy_id", {
        max: 64,
        pattern: VERSION,
      }),
      policy_version: text(
        deidentification.policy_version,
        "deidentification.policy_version",
        { max: 64, pattern: VERSION },
      ),
    },
    series,
  };
}

export function parseCompleteUploadRequest(
  value: unknown,
): CompleteUploadRequest {
  const input = record(value, "request");
  exactKeys(input, ["objects"], [], "request");
  if (
    !Array.isArray(input.objects) ||
    input.objects.length < 1 ||
    input.objects.length > MAX_COMPLETION_OBJECTS
  ) {
    invalid(`objects must contain 1-${MAX_COMPLETION_OBJECTS} entries`);
  }
  const keys = new Set<string>();
  const objects = input.objects.map((rawObject, index): CompletedObject => {
    const label = `objects[${index}]`;
    const object = record(rawObject, label);
    exactKeys(object, ["key", "size", "sha256", "parts"], [], label);
    const key = validateRelativeKey(object.key, `${label}.key`);
    if (keys.has(key)) invalid(`${label}.key must be unique`);
    keys.add(key);
    const result: CompletedObject = {
      key,
      size: integer(
        object.size,
        `${label}.size`,
        2,
        MAX_DICOM_ARCHIVE_BYTES,
      ),
      sha256: text(object.sha256, `${label}.sha256`, {
        max: 64,
        pattern: SHA256,
      }),
      parts: [],
    };
    if (
      !Array.isArray(object.parts) ||
      object.parts.length < 1 ||
      object.parts.length > 10_000
    ) {
      invalid(`${label}.parts must contain 1-10000 entries`);
    }
    result.parts = object.parts.map((rawPart, partIndex) => {
      const partLabel = `${label}.parts[${partIndex}]`;
      const part = record(rawPart, partLabel);
      exactKeys(part, ["part_number", "etag"], [], partLabel);
      const partNumber = integer(
        part.part_number,
        `${partLabel}.part_number`,
        1,
        10_000,
      );
      if (partNumber !== partIndex + 1) {
        invalid(
          `${label}.parts must be consecutive and sorted from part_number 1`,
        );
      }
      return {
        part_number: partNumber,
        etag: etag(part.etag, `${partLabel}.etag`),
      };
    });
    return result;
  });
  return { objects };
}

export function parseSignPartRequest(value: unknown): SignPartRequest {
  const input = record(value, "request");
  exactKeys(input, ["key", "part_number", "size", "sha256"], [], "request");
  return {
    key: validateRelativeKey(input.key, "key"),
    part_number: integer(input.part_number, "part_number", 1, 10_000),
    size: integer(input.size, "size", 1, MAX_NIFTI_BYTES),
    sha256: text(input.sha256, "sha256", { max: 64, pattern: SHA256 }),
  };
}

function leaseToken(value: unknown, label = "lease_token"): string {
  return text(value, label, { min: 36, max: 36, pattern: UUID });
}

function processorOutputDescriptor(
  value: unknown,
  label: string,
): ProcessorOutputDescriptor {
  const input = record(value, label);
  exactKeys(
    input,
    ["kind", "size_bytes", "sha256", "content_type"],
    ["uncompressed_sha256"],
    label,
  );
  if (!(["nifti", "sidecar", "processing_manifest"] as unknown[]).includes(input.kind)) {
    invalid(`${label}.kind is invalid`);
  }
  const kind = input.kind as ProcessorOutputKind;
  const expectedContentType =
    kind === "nifti" ? "application/gzip" : "application/json";
  if (input.content_type !== expectedContentType) {
    invalid(`${label}.content_type must be ${expectedContentType}`);
  }
  if (kind === "nifti" && input.uncompressed_sha256 === undefined) {
    invalid(`${label}.uncompressed_sha256 is required for nifti`);
  }
  if (kind !== "nifti" && input.uncompressed_sha256 !== undefined) {
    invalid(`${label}.uncompressed_sha256 is only valid for nifti`);
  }
  return {
    kind,
    size_bytes: integer(
      input.size_bytes,
      `${label}.size_bytes`,
      kind === "nifti" ? 32 : 2,
      kind === "nifti" ? MAX_NIFTI_BYTES : MAX_METADATA_BYTES,
    ),
    sha256: text(input.sha256, `${label}.sha256`, {
      max: 64,
      pattern: SHA256,
    }),
    content_type: expectedContentType,
    ...(kind === "nifti"
      ? {
          uncompressed_sha256: text(
            input.uncompressed_sha256,
            `${label}.uncompressed_sha256`,
            { max: 64, pattern: SHA256 },
          ),
        }
      : {}),
  };
}

function processorOutputs(
  value: unknown,
  label: string,
  minimum: number,
): ProcessorOutputDescriptor[] {
  if (!Array.isArray(value) || value.length < minimum || value.length > 3) {
    invalid(`${label} must contain ${minimum}-3 entries`);
  }
  const outputs = value.map((item, index) =>
    processorOutputDescriptor(item, `${label}[${index}]`),
  );
  if (new Set(outputs.map((output) => output.kind)).size !== outputs.length) {
    invalid(`${label} kinds must be unique`);
  }
  return outputs;
}

export function parseProcessorClaimRequest(
  value: unknown,
): ProcessorClaimRequest {
  const input = record(value, "request");
  exactKeys(
    input,
    ["processor_id", "lease_seconds"],
    [
      "claim_input_format",
      "processor_version",
      "pipeline_version",
      "controller_source_sha256",
    ],
    "request",
  );
  if (
    input.claim_input_format !== undefined &&
    input.claim_input_format !== "dicom-series-v1" &&
    input.claim_input_format !== "nifti-v1"
  ) {
    invalid("claim_input_format must be dicom-series-v1 or nifti-v1");
  }
  const attestationFields = [
    input.processor_version,
    input.pipeline_version,
    input.controller_source_sha256,
  ];
  if (
    attestationFields.some((value) => value !== undefined) &&
    !attestationFields.every((value) => value !== undefined)
  ) {
    invalid("processor readiness attestation must be complete");
  }
  return {
    processor_id: text(input.processor_id, "processor_id", {
      max: 96,
      pattern: PROCESSOR_ID,
    }),
    lease_seconds: integer(input.lease_seconds, "lease_seconds", 60, 3600),
    ...(input.claim_input_format === undefined
      ? {}
      : { claim_input_format: input.claim_input_format }),
    ...(input.processor_version === undefined
      ? {}
      : {
          processor_version: text(
            input.processor_version,
            "processor_version",
            { max: 64, pattern: CLIENT_VERSION },
          ),
          pipeline_version: text(
            input.pipeline_version,
            "pipeline_version",
            { max: 64, pattern: VERSION },
          ),
          controller_source_sha256: text(
            input.controller_source_sha256,
            "controller_source_sha256",
            { max: 64, pattern: SHA256 },
          ),
        }),
  };
}

export function parseProcessorLeaseRequest(
  value: unknown,
): ProcessorLeaseRequest {
  const input = record(value, "request");
  exactKeys(input, ["lease_token", "lease_seconds"], [], "request");
  return {
    lease_token: leaseToken(input.lease_token),
    lease_seconds: integer(input.lease_seconds, "lease_seconds", 60, 3600),
  };
}

export function parseProcessorOutputRequest(
  value: unknown,
): ProcessorOutputRequest {
  const input = record(value, "request");
  exactKeys(input, ["lease_token", "outputs"], [], "request");
  return {
    lease_token: leaseToken(input.lease_token),
    outputs: processorOutputs(input.outputs, "outputs", 3),
  };
}

export function parseProcessorCompleteRequest(
  value: unknown,
): ProcessorCompleteRequest {
  const input = record(value, "request");
  exactKeys(
    input,
    [
      "lease_token",
      "processor_version",
      "outputs",
      "validation",
    ],
    ["dcm2niix_version"],
    "request",
  );
  const validation = record(input.validation, "validation");
  let parsedValidation: DicomProcessorValidation | NiftiProcessorValidation;
  if ("archive_sha256_verified" in validation) {
    exactKeys(
      validation,
      [
        "archive_sha256_verified",
        "dicom_count",
        "dicom_parse_succeeded",
        "functional_epi_confirmed",
      ],
      ["dicom_privacy_audit_succeeded"],
      "validation",
    );
    parsedValidation = {
      archive_sha256_verified: boolean(
        validation.archive_sha256_verified,
        "validation.archive_sha256_verified",
      ),
      dicom_count: integer(
        validation.dicom_count,
        "validation.dicom_count",
        1,
        MAX_DICOM_INSTANCES_PER_SERIES,
      ),
      dicom_parse_succeeded: boolean(
        validation.dicom_parse_succeeded,
        "validation.dicom_parse_succeeded",
      ),
      ...(validation.dicom_privacy_audit_succeeded === undefined
        ? {}
        : {
            dicom_privacy_audit_succeeded: boolean(
              validation.dicom_privacy_audit_succeeded,
              "validation.dicom_privacy_audit_succeeded",
            ),
          }),
      functional_epi_confirmed: boolean(
        validation.functional_epi_confirmed,
        "validation.functional_epi_confirmed",
      ),
    };
  } else {
    exactKeys(
      validation,
      [
        "nifti_sha256_verified",
        "nifti_uncompressed_sha256_verified",
        "sidecar_sha256_verified",
        "nifti_header_valid",
        "sidecar_valid",
        "nifti_sidecar_consistent",
      ],
      [],
      "validation",
    );
    parsedValidation = {
      nifti_sha256_verified: boolean(
        validation.nifti_sha256_verified,
        "validation.nifti_sha256_verified",
      ),
      nifti_uncompressed_sha256_verified: boolean(
        validation.nifti_uncompressed_sha256_verified,
        "validation.nifti_uncompressed_sha256_verified",
      ),
      sidecar_sha256_verified: boolean(
        validation.sidecar_sha256_verified,
        "validation.sidecar_sha256_verified",
      ),
      nifti_header_valid: boolean(
        validation.nifti_header_valid,
        "validation.nifti_header_valid",
      ),
      sidecar_valid: boolean(validation.sidecar_valid, "validation.sidecar_valid"),
      nifti_sidecar_consistent: boolean(
        validation.nifti_sidecar_consistent,
        "validation.nifti_sidecar_consistent",
      ),
    };
  }
  return {
    lease_token: leaseToken(input.lease_token),
    processor_version: text(input.processor_version, "processor_version", {
      max: 64,
      pattern: VERSION,
    }),
    ...(input.dcm2niix_version === undefined
      ? {}
      : {
          dcm2niix_version: text(
            input.dcm2niix_version,
            "dcm2niix_version",
            { max: 64, pattern: VERSION },
          ),
        }),
    outputs: processorOutputs(input.outputs, "outputs", 0),
    validation: parsedValidation,
  };
}

export function parsePublicPolicyAcceptanceRequest(
  value: unknown,
): PublicPolicyAcceptanceRequest {
  const input = record(value, "request");
  exactKeys(input, ["accepted_consent_policy_version"], [], "request");
  return {
    accepted_consent_policy_version: text(
      input.accepted_consent_policy_version,
      "accepted_consent_policy_version",
      { max: 64, pattern: VERSION },
    ),
  };
}

export function parseProcessorFailRequest(
  value: unknown,
): ProcessorFailRequest {
  const input = record(value, "request");
  exactKeys(
    input,
    ["lease_token", "retryable", "error_code", "error_message"],
    [],
    "request",
  );
  const errorCode = text(input.error_code, "error_code", {
    max: 64,
    pattern: ERROR_CODE,
  });
  // This code is an internal Worker conclusion reached only after the final
  // retryable full-object digest mismatch. Accepting it from a processor would
  // let one report bypass the independent redownload threshold and trigger a
  // destructive source purge.
  if (errorCode === "STORED_OBJECT_SHA256_MISMATCH") {
    invalid("error_code is reserved for control-plane integrity reconciliation");
  }
  return {
    lease_token: leaseToken(input.lease_token),
    retryable: boolean(input.retryable, "retryable"),
    error_code: errorCode,
    error_message: humanLabel(input.error_message, "error_message", 512),
  };
}

export function parseAdminInviteRequest(value: unknown): AdminInviteRequest {
  const input = record(value, "request");
  exactKeys(
    input,
    [
      "site_slug",
      "site_name",
      "project_slug",
      "project_name",
      "consent_policy_version",
    ],
    ["expires_in_seconds", "max_uses"],
    "request",
  );
  return {
    site_slug: text(input.site_slug, "site_slug", { max: 63, pattern: SLUG }),
    site_name: humanLabel(input.site_name, "site_name", 128),
    project_slug: text(input.project_slug, "project_slug", {
      max: 63,
      pattern: SLUG,
    }),
    project_name: humanLabel(input.project_name, "project_name", 128),
    consent_policy_version: text(
      input.consent_policy_version,
      "consent_policy_version",
      { max: 64, pattern: VERSION },
    ),
    expires_in_seconds:
      input.expires_in_seconds === undefined
        ? 7 * 24 * 60 * 60
        : integer(
            input.expires_in_seconds,
            "expires_in_seconds",
            900,
            30 * 24 * 60 * 60,
          ),
    max_uses:
      input.max_uses === undefined
        ? 1
        : integer(input.max_uses, "max_uses", 1, 100),
  };
}

export function parseJsonText(textValue: string): unknown {
  if (textValue.length === 0) invalid("request body is required");
  if (textValue.length > 1024 * 1024) invalid("request body exceeds 1 MiB");
  try {
    return JSON.parse(textValue) as unknown;
  } catch {
    invalid("request body must be valid JSON");
  }
}
