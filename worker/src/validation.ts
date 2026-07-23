import { invalid } from "./errors";

const PSEUDONYM_ID = /^[a-f0-9]{24}$/u;
const UUID =
  /^[a-f0-9]{8}-[a-f0-9]{4}-[1-8][a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/u;
const VERSION = /^[A-Za-z0-9][A-Za-z0-9.+_-]{0,63}$/u;
const CLIENT_VERSION =
  /^(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)(?:-[A-Za-z0-9.-]+)?(?:\+[A-Za-z0-9.-]+)?$/u;
const SHA256 = /^[a-f0-9]{64}$/u;
const ETAG = /^[A-Za-z0-9+/=_:.-]{1,256}$/u;
const PLATFORM = /^[A-Za-z0-9][A-Za-z0-9._-]{0,31}$/u;
const EMAIL =
  /^[A-Za-z0-9.!#$%&'*+/=?^_`{|}~-]+@[A-Za-z0-9](?:[A-Za-z0-9.-]{0,251}[A-Za-z0-9])?$/u;
const ROR_ID = /^https:\/\/ror\.org\/0[a-hj-km-np-tv-z0-9]{8}$/u;
const MAX_DICOM_INSTANCES = 500_000;
const MAX_DICOM_ARCHIVE_BYTES = 64 * 1024 ** 3;

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

export interface PublicPolicyAcceptanceRequest {
  accepted_consent_policy_version: string;
}

export interface DicomSeriesDescriptor {
  series_archive_id: string;
  series_id: string;
  subject_id: string;
  session_id: string;
  protocol_group_id: string;
  dicom_count: number;
  series_kind: "functional_epi";
  archive_route: "functional-epi-v1";
  pixel_data_policy: "scanner-native-not-defaced";
  archive: {
    format: "dicom-tar-zstd";
    relative_key: string;
    size: number;
    sha256: string;
  };
}

export interface CreateDicomUploadRequest {
  format: "dicom-series-v1";
  client_version: string;
  deidentification: { policy_id: string; policy_version: string };
  series: [DicomSeriesDescriptor];
}

export interface CompletedPart {
  part_number: number;
  etag: string;
}

export interface CompletedObject {
  key: string;
  size: number;
  sha256: string;
  parts: CompletedPart[];
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

function record(value: unknown, label: string): Record<string, unknown> {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    invalid(`${label} must be an object`);
  }
  return value as Record<string, unknown>;
}

function exactKeys(
  value: Record<string, unknown>,
  required: readonly string[],
  optional: readonly string[],
  label: string,
): void {
  const allowed = new Set([...required, ...optional]);
  for (const key of Object.keys(value)) {
    if (!allowed.has(key)) invalid(`${label}.${key} is not allowed`);
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
  if (typeof value !== "string") invalid(`${label} must be text`);
  if (
    value.length < (options.min ?? 1) ||
    value.length > options.max ||
    (options.pattern && !options.pattern.test(value))
  ) {
    invalid(`${label} is invalid`);
  }
  return value;
}

function humanLabel(value: unknown, label: string, maximum: number): string {
  const normalized = text(value, label, { max: maximum })
    .trim()
    .replace(/\s+/gu, " ");
  if (normalized.length < 2) invalid(`${label} is too short`);
  return normalized;
}

function booleanValue(value: unknown, label: string): boolean {
  if (typeof value !== "boolean") invalid(`${label} must be a boolean`);
  return value;
}

function integer(
  value: unknown,
  label: string,
  minimum: number,
  maximum: number,
): number {
  if (
    typeof value !== "number" ||
    !Number.isSafeInteger(value) ||
    value < minimum ||
    value > maximum
  ) {
    invalid(`${label} is outside the supported range`);
  }
  return value;
}

function pseudonymId(value: unknown, label: string): string {
  return text(value, label, { min: 24, max: 24, pattern: PSEUDONYM_ID });
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
  if (contactEmail.includes("..")) invalid("contact_email is invalid");
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
    platform: text(input.platform, "platform", {
      max: 32,
      pattern: PLATFORM,
    }),
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
    contact_opt_in: booleanValue(input.contact_opt_in, "contact_opt_in"),
    accepted_consent_policy_version: text(
      input.accepted_consent_policy_version,
      "accepted_consent_policy_version",
      { max: 64, pattern: VERSION },
    ),
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
  if (!Array.isArray(input.series) || input.series.length !== 1) {
    invalid("series must contain exactly one functional EPI archive");
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

  const item = record(input.series[0], "series[0]");
  exactKeys(
    item,
    [
      "series_archive_id",
      "series_id",
      "subject_id",
      "session_id",
      "protocol_group_id",
      "dicom_count",
      "series_kind",
      "archive_route",
      "pixel_data_policy",
      "archive",
    ],
    [],
    "series[0]",
  );
  if (
    item.series_kind !== "functional_epi" ||
    item.archive_route !== "functional-epi-v1" ||
    item.pixel_data_policy !== "scanner-native-not-defaced"
  ) {
    invalid("series[0] must use the functional EPI archive contract");
  }

  const seriesArchiveId = pseudonymId(
    item.series_archive_id,
    "series[0].series_archive_id",
  );
  const archive = record(item.archive, "series[0].archive");
  exactKeys(
    archive,
    ["format", "relative_key", "size", "sha256"],
    [],
    "series[0].archive",
  );
  if (archive.format !== "dicom-tar-zstd") {
    invalid("series[0].archive.format must be dicom-tar-zstd");
  }
  const relativeKey = text(
    archive.relative_key,
    "series[0].archive.relative_key",
    { max: 512, pattern: /^[A-Za-z0-9._~/-]+$/u },
  );
  if (relativeKey !== `${seriesArchiveId}/dicom.tar.zst`) {
    invalid("series[0].archive.relative_key is not canonical");
  }

  const descriptor: DicomSeriesDescriptor = {
    series_archive_id: seriesArchiveId,
    series_id: pseudonymId(item.series_id, "series[0].series_id"),
    subject_id: pseudonymId(item.subject_id, "series[0].subject_id"),
    session_id: pseudonymId(item.session_id, "series[0].session_id"),
    protocol_group_id: pseudonymId(
      item.protocol_group_id,
      "series[0].protocol_group_id",
    ),
    dicom_count: integer(
      item.dicom_count,
      "series[0].dicom_count",
      1,
      MAX_DICOM_INSTANCES,
    ),
    series_kind: "functional_epi",
    archive_route: "functional-epi-v1",
    pixel_data_policy: "scanner-native-not-defaced",
    archive: {
      format: "dicom-tar-zstd",
      relative_key: relativeKey,
      size: integer(
        archive.size,
        "series[0].archive.size",
        32,
        MAX_DICOM_ARCHIVE_BYTES,
      ),
      sha256: text(archive.sha256, "series[0].archive.sha256", {
        min: 64,
        max: 64,
        pattern: SHA256,
      }),
    },
  };
  return {
    format: "dicom-series-v1",
    client_version: text(input.client_version, "client_version", {
      max: 64,
      pattern: CLIENT_VERSION,
    }),
    deidentification: {
      policy_id: text(
        deidentification.policy_id,
        "deidentification.policy_id",
        { max: 64, pattern: VERSION },
      ),
      policy_version: text(
        deidentification.policy_version,
        "deidentification.policy_version",
        { max: 64, pattern: VERSION },
      ),
    },
    series: [descriptor],
  };
}

export function parseCompleteUploadRequest(
  value: unknown,
): CompleteUploadRequest {
  const input = record(value, "request");
  exactKeys(input, ["objects"], [], "request");
  if (!Array.isArray(input.objects) || input.objects.length > 1) {
    invalid("objects must contain at most one functional EPI archive");
  }
  return {
    objects: input.objects.map((raw, index) => {
      const label = `objects[${index}]`;
      const item = record(raw, label);
      exactKeys(item, ["key", "size", "sha256", "parts"], [], label);
      if (!Array.isArray(item.parts) || item.parts.length < 1) {
        invalid(`${label}.parts must not be empty`);
      }
      const parts = item.parts.map((rawPart, partIndex) => {
        const partLabel = `${label}.parts[${partIndex}]`;
        const part = record(rawPart, partLabel);
        exactKeys(part, ["part_number", "etag"], [], partLabel);
        return {
          part_number: integer(
            part.part_number,
            `${partLabel}.part_number`,
            1,
            10_000,
          ),
          etag: text(part.etag, `${partLabel}.etag`, {
            max: 256,
            pattern: ETAG,
          }),
        };
      });
      if (
        new Set(parts.map((part) => part.part_number)).size !== parts.length ||
        parts.some((part, partIndex) => part.part_number !== partIndex + 1)
      ) {
        invalid(`${label}.parts must be unique and sequential`);
      }
      return {
        key: text(item.key, `${label}.key`, {
          max: 1024,
          pattern: /^[A-Za-z0-9._~/-]+$/u,
        }),
        size: integer(item.size, `${label}.size`, 32, MAX_DICOM_ARCHIVE_BYTES),
        sha256: text(item.sha256, `${label}.sha256`, {
          min: 64,
          max: 64,
          pattern: SHA256,
        }),
        parts,
      };
    }),
  };
}

export function parseSignPartRequest(value: unknown): SignPartRequest {
  const input = record(value, "request");
  exactKeys(input, ["key", "part_number", "size", "sha256"], [], "request");
  return {
    key: text(input.key, "key", {
      max: 1024,
      pattern: /^[A-Za-z0-9._~/-]+$/u,
    }),
    part_number: integer(input.part_number, "part_number", 1, 10_000),
    size: integer(input.size, "size", 1, MAX_DICOM_ARCHIVE_BYTES),
    sha256: text(input.sha256, "sha256", {
      min: 64,
      max: 64,
      pattern: SHA256,
    }),
  };
}

export function parseJsonText(textValue: string): unknown {
  try {
    return JSON.parse(textValue) as unknown;
  } catch {
    invalid("Request body must be valid JSON");
  }
}
