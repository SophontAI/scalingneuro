import { canonicalJson } from "./crypto";
import type { Env } from "./env";
import { AppError } from "./errors";

const encoder = new TextEncoder();
const LAUNCH_TIMEOUT_MS = 30_000;

interface LaunchableUpload {
  cluster_launch_dispatched_at: number | null;
}

function decodeHmacKey(value: string): Uint8Array<ArrayBuffer> {
  try {
    const binary = atob(value);
    const bytes = new Uint8Array(binary.length);
    for (let index = 0; index < binary.length; index += 1) {
      bytes[index] = binary.charCodeAt(index);
    }
    if (bytes.byteLength !== 32) throw new Error("invalid key length");
    return bytes;
  } catch {
    throw new AppError(
      "INTERNAL",
      500,
      "Cluster launch signing key configuration is invalid",
    );
  }
}

function bytesToHex(bytes: Uint8Array): string {
  let output = "";
  for (const byte of bytes) output += byte.toString(16).padStart(2, "0");
  return output;
}

function launchUrl(value: string): string {
  let parsed: URL;
  try {
    parsed = new URL(value);
  } catch {
    throw new AppError("INTERNAL", 500, "Cluster launch URL is invalid");
  }
  const localTestOrigin =
    parsed.protocol === "http:" &&
    ["127.0.0.1", "localhost"].includes(parsed.hostname);
  if (
    (parsed.protocol !== "https:" && !localTestOrigin) ||
    parsed.username ||
    parsed.password ||
    parsed.search ||
    parsed.hash ||
    parsed.pathname !== "/v1/launch"
  ) {
    throw new AppError("INTERNAL", 500, "Cluster launch URL is invalid");
  }
  return parsed.toString();
}

async function signature(
  keyValue: string,
  timestamp: string,
  nonce: string,
  body: string,
): Promise<string> {
  const key = await crypto.subtle.importKey(
    "raw",
    decodeHmacKey(keyValue),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"],
  );
  const signed = await crypto.subtle.sign(
    "HMAC",
    key,
    encoder.encode(`${timestamp}\n${nonce}\n${body}`),
  );
  return `v1=${bytesToHex(new Uint8Array(signed))}`;
}

export async function buildClusterLaunchRequest(
  env: Env,
  uploadId: string,
  timestamp = Math.floor(Date.now() / 1000).toString(),
  nonce = crypto.randomUUID(),
): Promise<Request> {
  const body = canonicalJson({
    event: "dicom-upload-committed",
    upload_id: uploadId,
  });
  return new Request(launchUrl(env.CLUSTER_LAUNCH_URL), {
    method: "POST",
    headers: {
      "content-type": "application/json",
      "x-scaling-neuro-nonce": nonce,
      "x-scaling-neuro-signature": await signature(
        env.CLUSTER_LAUNCH_HMAC_KEY,
        timestamp,
        nonce,
        body,
      ),
      "x-scaling-neuro-timestamp": timestamp,
    },
    body,
  });
}

export async function ensureClusterLaunch(
  env: Env,
  uploadId: string,
  request: typeof fetch = fetch,
): Promise<void> {
  const upload = await env.DB.prepare(
    `SELECT cluster_launch_dispatched_at
     FROM uploads
     WHERE id = ?1 AND status = 'committed'
       AND ingest_format = 'dicom-series-v1'
       AND EXISTS (
         SELECT 1 FROM processing_jobs j
         WHERE j.upload_id = uploads.id
           AND j.status IN ('queued', 'processing')
       )`,
  )
    .bind(uploadId)
    .first<LaunchableUpload>();
  if (!upload || upload.cluster_launch_dispatched_at !== null) return;

  let response: Response;
  try {
    response = await request(await buildClusterLaunchRequest(env, uploadId), {
      signal: AbortSignal.timeout(LAUNCH_TIMEOUT_MS),
    });
  } catch {
    throw new AppError(
      "STORAGE_UNAVAILABLE",
      502,
      "Sophont processing could not be queued; retry shortly",
    );
  }
  if (!response.ok) {
    throw new AppError(
      "STORAGE_UNAVAILABLE",
      502,
      "Sophont processing could not be queued; retry shortly",
    );
  }

  await env.DB.prepare(
    `UPDATE uploads SET cluster_launch_dispatched_at = ?1
     WHERE id = ?2 AND cluster_launch_dispatched_at IS NULL`,
  )
    .bind(Math.floor(Date.now() / 1000), uploadId)
    .run();
}
