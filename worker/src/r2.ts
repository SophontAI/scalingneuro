import { AppError } from "./errors";
import type { Env } from "./env";
import { sha256Hex, utf8Bytes } from "./crypto";

export interface PresignedUploadPart {
  url: string;
  headers: Record<"content-length" | "x-amz-content-sha256", string>;
  expires_at: string;
}

export interface PresignedDownload {
  url: string;
  expires_at: string;
}

function boundedInteger(
  value: string | undefined,
  fallback: number,
  min: number,
  max: number,
): number {
  if (value === undefined) return fallback;
  const parsed = Number(value);
  if (!Number.isInteger(parsed) || parsed < min || parsed > max) {
    throw new AppError("INTERNAL", 500, "Worker TTL configuration is invalid");
  }
  return parsed;
}

export function credentialTtl(env: Env): number {
  // The desktop client intentionally refuses broader grants. Keep this fixed
  // instead of allowing an environment override to weaken or brick the wire
  // contract.
  return boundedInteger(env.CREDENTIAL_TTL_SECONDS, 900, 900, 900);
}

export function uploadTtl(env: Env): number {
  return boundedInteger(env.UPLOAD_TTL_SECONDS, 86_400, 3_600, 2_592_000);
}

function awsEncode(value: string): string {
  return encodeURIComponent(value).replace(/[!'()*]/gu, (character) =>
    `%${character.charCodeAt(0).toString(16).toUpperCase()}`,
  );
}

function hex(bytes: Uint8Array): string {
  return [...bytes]
    .map((value) => value.toString(16).padStart(2, "0"))
    .join("");
}

async function hmacSha256(
  key: string | Uint8Array<ArrayBuffer>,
  value: string,
): Promise<Uint8Array<ArrayBuffer>> {
  const rawKey = typeof key === "string" ? utf8Bytes(key) : key;
  const cryptoKey = await crypto.subtle.importKey(
    "raw",
    rawKey,
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"],
  );
  return new Uint8Array(
    await crypto.subtle.sign("HMAC", cryptoKey, utf8Bytes(value)),
  );
}

export async function presignUploadPart(
  env: Env,
  input: {
    key: string;
    uploadId: string;
    partNumber: number;
    size: number;
    sha256: string;
  },
  issuedAt = new Date(),
): Promise<PresignedUploadPart> {
  if (
    !env.R2_ACCOUNT_ID ||
    !env.R2_PARENT_ACCESS_KEY_ID ||
    !env.R2_PARENT_SECRET_ACCESS_KEY ||
    !env.R2_BUCKET_NAME
  ) {
    throw new AppError(
      "CREDENTIALS_UNAVAILABLE",
      503,
      "Part URL signing is not configured",
    );
  }

  const ttlSeconds = credentialTtl(env);
  const expiresAt = new Date(issuedAt.getTime() + ttlSeconds * 1000);
  const host = `${env.R2_ACCOUNT_ID}.r2.cloudflarestorage.com`;
  const amzDate = issuedAt
    .toISOString()
    .replace(/[:-]|\.\d{3}/gu, "");
  const date = amzDate.slice(0, 8);
  const region = "auto";
  const service = "s3";
  const scope = `${date}/${region}/${service}/aws4_request`;
  const signedHeaders = "content-length;host;x-amz-content-sha256";
  const query = new Map<string, string>([
    ["X-Amz-Algorithm", "AWS4-HMAC-SHA256"],
    ["X-Amz-Credential", `${env.R2_PARENT_ACCESS_KEY_ID}/${scope}`],
    ["X-Amz-Date", amzDate],
    ["X-Amz-Expires", String(ttlSeconds)],
    ["X-Amz-SignedHeaders", signedHeaders],
    ["partNumber", String(input.partNumber)],
    ["uploadId", input.uploadId],
  ]);
  const canonicalQuery = [...query.entries()]
    .map(([key, value]) => [awsEncode(key), awsEncode(value)] as const)
    .sort(([left], [right]) => (left < right ? -1 : left > right ? 1 : 0))
    .map(([key, value]) => `${key}=${value}`)
    .join("&");
  const canonicalUri = `/${awsEncode(env.R2_BUCKET_NAME)}/${input.key
    .split("/")
    .map(awsEncode)
    .join("/")}`;
  const canonicalHeaders =
    `content-length:${input.size}\n` +
    `host:${host}\n` +
    `x-amz-content-sha256:${input.sha256}\n`;
  const canonicalRequest = [
    "PUT",
    canonicalUri,
    canonicalQuery,
    canonicalHeaders,
    signedHeaders,
    // R2 follows the S3 query-auth convention here: the canonical payload is
    // UNSIGNED-PAYLOAD even though the actual digest header is itself signed.
    // A live R2 regression smoke verifies both a successful PUT and rejection
    // of same-length bytes that do not match x-amz-content-sha256.
    "UNSIGNED-PAYLOAD",
  ].join("\n");
  const stringToSign = [
    "AWS4-HMAC-SHA256",
    amzDate,
    scope,
    await sha256Hex(canonicalRequest),
  ].join("\n");

  let signature: string;
  try {
    const dateKey = await hmacSha256(
      `AWS4${env.R2_PARENT_SECRET_ACCESS_KEY}`,
      date,
    );
    const regionKey = await hmacSha256(dateKey, region);
    const serviceKey = await hmacSha256(regionKey, service);
    const signingKey = await hmacSha256(serviceKey, "aws4_request");
    signature = hex(await hmacSha256(signingKey, stringToSign));
  } catch {
    throw new AppError(
      "CREDENTIALS_UNAVAILABLE",
      503,
      "Unable to sign upload part URL",
    );
  }

  return {
    url: `https://${host}${canonicalUri}?${canonicalQuery}&X-Amz-Signature=${signature}`,
    headers: {
      "content-length": String(input.size),
      "x-amz-content-sha256": input.sha256,
    },
    expires_at: expiresAt.toISOString(),
  };
}

export async function presignGetObject(
  env: Env,
  key: string,
  issuedAt = new Date(),
): Promise<PresignedDownload> {
  if (
    !env.R2_ACCOUNT_ID ||
    !env.R2_PARENT_ACCESS_KEY_ID ||
    !env.R2_PARENT_SECRET_ACCESS_KEY ||
    !env.R2_BUCKET_NAME
  ) {
    throw new AppError(
      "CREDENTIALS_UNAVAILABLE",
      503,
      "Object URL signing is not configured",
    );
  }

  const ttlSeconds = credentialTtl(env);
  const expiresAt = new Date(issuedAt.getTime() + ttlSeconds * 1000);
  const host = `${env.R2_ACCOUNT_ID}.r2.cloudflarestorage.com`;
  const amzDate = issuedAt.toISOString().replace(/[:-]|\.\d{3}/gu, "");
  const date = amzDate.slice(0, 8);
  const scope = `${date}/auto/s3/aws4_request`;
  const signedHeaders = "host";
  const canonicalHeaders = `host:${host}\n`;
  const query = new Map<string, string>([
    ["X-Amz-Algorithm", "AWS4-HMAC-SHA256"],
    ["X-Amz-Credential", `${env.R2_PARENT_ACCESS_KEY_ID}/${scope}`],
    ["X-Amz-Date", amzDate],
    ["X-Amz-Expires", String(ttlSeconds)],
    ["X-Amz-SignedHeaders", signedHeaders],
  ]);
  const canonicalQuery = [...query.entries()]
    .map(([key, value]) => [awsEncode(key), awsEncode(value)] as const)
    .sort(([left], [right]) => (left < right ? -1 : left > right ? 1 : 0))
    .map(([key, value]) => `${key}=${value}`)
    .join("&");
  const canonicalUri = `/${awsEncode(env.R2_BUCKET_NAME)}/${key
    .split("/")
    .map(awsEncode)
    .join("/")}`;
  const canonicalRequest = [
    "GET",
    canonicalUri,
    canonicalQuery,
    canonicalHeaders,
    signedHeaders,
    "UNSIGNED-PAYLOAD",
  ].join("\n");
  const stringToSign = [
    "AWS4-HMAC-SHA256",
    amzDate,
    scope,
    await sha256Hex(canonicalRequest),
  ].join("\n");
  try {
    const dateKey = await hmacSha256(
      `AWS4${env.R2_PARENT_SECRET_ACCESS_KEY}`,
      date,
    );
    const regionKey = await hmacSha256(dateKey, "auto");
    const serviceKey = await hmacSha256(regionKey, "s3");
    const signingKey = await hmacSha256(serviceKey, "aws4_request");
    const signature = hex(await hmacSha256(signingKey, stringToSign));
    return {
      url: `https://${host}${canonicalUri}?${canonicalQuery}&X-Amz-Signature=${signature}`,
      expires_at: expiresAt.toISOString(),
    };
  } catch {
    throw new AppError(
      "CREDENTIALS_UNAVAILABLE",
      503,
      "Unable to sign object URL",
    );
  }
}
