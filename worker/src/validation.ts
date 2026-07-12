import { invalid } from "./errors";

const PSEUDONYM_ID = /^[a-f0-9]{24}$/u;
const UUID =
  /^[a-f0-9]{8}-[a-f0-9]{4}-[1-8][a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/u;
const SLUG = /^[a-z0-9][a-z0-9-]{0,62}$/u;
const VERSION = /^[A-Za-z0-9][A-Za-z0-9.+_-]{0,63}$/u;
const SHA256 = /^[a-f0-9]{64}$/u;
const ETAG = /^[A-Za-z0-9+/=_:.-]{1,256}$/u;
const PLATFORM = /^[A-Za-z0-9][A-Za-z0-9._-]{0,31}$/u;
const RELATIVE_KEY = /^[A-Za-z0-9._~/-]{1,512}$/u;
const MAX_BUNDLES = 32;
const MAX_NIFTI_BYTES = 5 * 1024 ** 3;
const MAX_METADATA_BYTES = 8 * 1024 ** 2;
const MAX_UPLOAD_BYTES = 32 * 1024 ** 3;

export interface EnrollRequest {
  invite_code: string;
  enrollment_id: string;
  device_token: string;
  device_name: string;
  client_version: string;
  platform: string;
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
      pattern: VERSION,
    }),
    platform: text(input.platform, "platform", { max: 32, pattern: PLATFORM }),
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
      pattern: VERSION,
    }),
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
    input.objects.length > MAX_BUNDLES * 2
  ) {
    invalid(`objects must contain 1-${MAX_BUNDLES * 2} entries`);
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
      size: integer(object.size, `${label}.size`, 2, MAX_NIFTI_BYTES),
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
