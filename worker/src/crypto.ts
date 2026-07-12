import { AppError } from "./errors";

const encoder = new TextEncoder();
const decoder = new TextDecoder();

function bytesToHex(bytes: Uint8Array): string {
  let output = "";
  for (const byte of bytes) output += byte.toString(16).padStart(2, "0");
  return output;
}

function bytesToBase64(bytes: Uint8Array): string {
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary);
}

function base64ToBytes(value: string): Uint8Array<ArrayBuffer> {
  try {
    const binary = atob(value);
    const bytes = new Uint8Array(binary.length);
    for (let index = 0; index < binary.length; index += 1) {
      bytes[index] = binary.charCodeAt(index);
    }
    return bytes;
  } catch {
    throw new AppError(
      "INTERNAL",
      500,
      "Encryption key configuration is invalid",
    );
  }
}

function base64Url(bytes: Uint8Array): string {
  return bytesToBase64(bytes)
    .replaceAll("+", "-")
    .replaceAll("/", "_")
    .replace(/=+$/u, "");
}

function fromBase64Url(value: string): Uint8Array<ArrayBuffer> {
  const standard = value.replaceAll("-", "+").replaceAll("_", "/");
  return base64ToBytes(
    standard.padEnd(Math.ceil(standard.length / 4) * 4, "="),
  );
}

export function randomBytes(length: number): Uint8Array<ArrayBuffer> {
  return crypto.getRandomValues(new Uint8Array(length));
}

export function randomOpaqueToken(prefix: "sn_device" | "sn_invite"): string {
  return `${prefix}_${base64Url(randomBytes(32))}`;
}

export function pseudonymKeyBase64(bytes: Uint8Array): string {
  return bytesToBase64(bytes);
}

export async function sha256Hex(
  value: string | Uint8Array<ArrayBuffer>,
): Promise<string> {
  const bytes = typeof value === "string" ? encoder.encode(value) : value;
  const digest = await crypto.subtle.digest("SHA-256", bytes);
  return bytesToHex(new Uint8Array(digest));
}

/**
 * Hash a body without buffering it in Worker memory. DigestStream is a
 * Cloudflare runtime primitive backed by native crypto, so this remains safe
 * for multi-gigabyte EPI objects under the Workers memory limit.
 */
export async function sha256StreamHex(
  body: ReadableStream<Uint8Array>,
): Promise<string> {
  // The Workers runtime exposes DigestStream as a Crypto extension. The cast
  // bridges the duplicate WebWorker/Workers Crypto declarations in TypeScript.
  const workersCrypto = crypto as Crypto & {
    DigestStream: typeof DigestStream;
  };
  const digestStream = new workersCrypto.DigestStream("SHA-256");
  await body.pipeTo(digestStream);
  return bytesToHex(new Uint8Array(await digestStream.digest));
}

export async function constantTimeEqual(
  left: string,
  right: string,
): Promise<boolean> {
  const [leftHash, rightHash] = await Promise.all([
    sha256Hex(left),
    sha256Hex(right),
  ]);
  let difference = leftHash.length ^ rightHash.length;
  for (let index = 0; index < leftHash.length; index += 1) {
    difference |=
      leftHash.charCodeAt(index) ^ (rightHash.charCodeAt(index) || 0);
  }
  return difference === 0;
}

export function canonicalJson(value: unknown): string {
  if (value === null || typeof value !== "object") return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;

  const record = value as Record<string, unknown>;
  const keys = Object.keys(record).sort();
  return `{${keys
    .map((key) => `${JSON.stringify(key)}:${canonicalJson(record[key])}`)
    .join(",")}}`;
}

async function importEncryptionKey(base64Key: string): Promise<CryptoKey> {
  const raw = base64ToBytes(base64Key);
  if (raw.byteLength !== 32) {
    throw new AppError(
      "INTERNAL",
      500,
      "Encryption key configuration is invalid",
    );
  }
  return crypto.subtle.importKey("raw", raw, "AES-GCM", false, [
    "encrypt",
    "decrypt",
  ]);
}

export async function encryptSiteKey(
  siteKey: Uint8Array<ArrayBuffer>,
  siteId: string,
  base64EncryptionKey: string,
): Promise<string> {
  const nonce = randomBytes(12);
  const key = await importEncryptionKey(base64EncryptionKey);
  const ciphertext = await crypto.subtle.encrypt(
    {
      name: "AES-GCM",
      iv: nonce,
      additionalData: encoder.encode(`scaling-neuro/site-key/v1/${siteId}`),
    },
    key,
    siteKey,
  );
  return `v1.${base64Url(nonce)}.${base64Url(new Uint8Array(ciphertext))}`;
}

export async function decryptSiteKey(
  ciphertext: string,
  siteId: string,
  base64EncryptionKey: string,
): Promise<Uint8Array<ArrayBuffer>> {
  const parts = ciphertext.split(".");
  const version = parts[0];
  const nonceValue = parts[1];
  const encryptedValue = parts[2];
  if (
    parts.length !== 3 ||
    version !== "v1" ||
    !nonceValue ||
    !encryptedValue
  ) {
    throw new AppError("INTERNAL", 500, "Encrypted site key is invalid");
  }

  try {
    const key = await importEncryptionKey(base64EncryptionKey);
    const plaintext = await crypto.subtle.decrypt(
      {
        name: "AES-GCM",
        iv: fromBase64Url(nonceValue),
        additionalData: encoder.encode(`scaling-neuro/site-key/v1/${siteId}`),
      },
      key,
      fromBase64Url(encryptedValue),
    );
    const result = new Uint8Array(plaintext);
    if (result.byteLength !== 32) throw new Error("invalid site key length");
    return result;
  } catch (error) {
    if (error instanceof AppError) throw error;
    throw new AppError("INTERNAL", 500, "Unable to decrypt site key");
  }
}

export function utf8Bytes(value: string): Uint8Array<ArrayBuffer> {
  return encoder.encode(value) as Uint8Array<ArrayBuffer>;
}

export function utf8String(value: ArrayBuffer): string {
  return decoder.decode(value);
}
