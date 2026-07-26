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

export function randomAccessToken(): string {
  return `sn_access_${base64Url(randomBytes(32))}`;
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

/**
 * Hash a body while forwarding each chunk to one downstream consumer. Unlike
 * ReadableStream.tee(), this applies backpressure before forwarding the next
 * chunk, so a fast hash branch cannot buffer an entire MRI object while the
 * gzip validator is still consuming it.
 */
export function sha256PassThrough(body: ReadableStream<Uint8Array>): {
  body: ReadableStream<Uint8Array>;
  sha256: Promise<string>;
} {
  const workersCrypto = crypto as Crypto & {
    DigestStream: typeof DigestStream;
  };
  const digestStream = new workersCrypto.DigestStream("SHA-256");
  const writer = digestStream.getWriter();
  const forwarded = body.pipeThrough(
    new TransformStream<Uint8Array, Uint8Array>({
      async transform(chunk, controller) {
        await writer.write(chunk);
        controller.enqueue(chunk);
      },
      async flush() {
        await writer.close();
      },
    }),
  );
  return {
    body: forwarded,
    sha256: digestStream.digest.then((digest) =>
      bytesToHex(new Uint8Array(digest)),
    ),
  };
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

async function encryptBoundText(
  plaintext: string,
  binding: string,
  base64EncryptionKey: string,
): Promise<string> {
  const nonce = randomBytes(12);
  const key = await importEncryptionKey(base64EncryptionKey);
  const ciphertext = await crypto.subtle.encrypt(
    {
      name: "AES-GCM",
      iv: nonce,
      additionalData: encoder.encode(binding),
    },
    key,
    encoder.encode(plaintext),
  );
  return `v1.${base64Url(nonce)}.${base64Url(new Uint8Array(ciphertext))}`;
}

async function decryptBoundText(
  ciphertext: string,
  binding: string,
  base64EncryptionKey: string,
): Promise<string> {
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
    throw new AppError("INTERNAL", 500, "Encrypted value is invalid");
  }
  try {
    const key = await importEncryptionKey(base64EncryptionKey);
    const plaintext = await crypto.subtle.decrypt(
      {
        name: "AES-GCM",
        iv: fromBase64Url(nonceValue),
        additionalData: encoder.encode(binding),
      },
      key,
      fromBase64Url(encryptedValue),
    );
    return decoder.decode(plaintext);
  } catch (error) {
    if (error instanceof AppError) throw error;
    throw new AppError("INTERNAL", 500, "Unable to decrypt protected value");
  }
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

export async function encryptRegistrationEmail(
  email: string,
  registrationId: string,
  base64EncryptionKey: string,
): Promise<string> {
  return encryptBoundText(
    email,
    `scaling-neuro/contributor-registration/v1/${registrationId}`,
    base64EncryptionKey,
  );
}

export async function decryptRegistrationEmail(
  ciphertext: string,
  registrationId: string,
  base64EncryptionKey: string,
): Promise<string> {
  return decryptBoundText(
    ciphertext,
    `scaling-neuro/contributor-registration/v1/${registrationId}`,
    base64EncryptionKey,
  );
}

export async function encryptArchiveAccessEmail(
  email: string,
  registrationId: string,
  base64EncryptionKey: string,
): Promise<string> {
  return encryptBoundText(
    email,
    `scaling-neuro/archive-access/v1/${registrationId}`,
    base64EncryptionKey,
  );
}

export async function decryptArchiveAccessEmail(
  ciphertext: string,
  registrationId: string,
  base64EncryptionKey: string,
): Promise<string> {
  return decryptBoundText(
    ciphertext,
    `scaling-neuro/archive-access/v1/${registrationId}`,
    base64EncryptionKey,
  );
}

export async function encryptArchiveAccessRequestEmail(
  email: string,
  requestId: string,
  base64EncryptionKey: string,
): Promise<string> {
  return encryptBoundText(
    email,
    `scaling-neuro/archive-access-request/v1/${requestId}`,
    base64EncryptionKey,
  );
}

export async function decryptArchiveAccessRequestEmail(
  ciphertext: string,
  requestId: string,
  base64EncryptionKey: string,
): Promise<string> {
  return decryptBoundText(
    ciphertext,
    `scaling-neuro/archive-access-request/v1/${requestId}`,
    base64EncryptionKey,
  );
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
