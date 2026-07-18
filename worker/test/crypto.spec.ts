import { describe, expect, it } from "vitest";
import {
  canonicalJson,
  decryptRegistrationEmail,
  decryptSiteKey,
  encryptRegistrationEmail,
  encryptSiteKey,
  randomBytes,
  sha256Hex,
  sha256PassThrough,
  sha256StreamHex,
} from "../src/crypto";

const ENCRYPTION_KEY = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";

describe("cryptographic helpers", () => {
  it("encrypts site pseudonym keys with site-bound authenticated encryption", async () => {
    const siteKey = randomBytes(32);
    const ciphertext = await encryptSiteKey(siteKey, "site-a", ENCRYPTION_KEY);

    expect(ciphertext).not.toContain(await sha256Hex(siteKey));
    expect(await decryptSiteKey(ciphertext, "site-a", ENCRYPTION_KEY)).toEqual(
      siteKey,
    );
    await expect(
      decryptSiteKey(ciphertext, "site-b", ENCRYPTION_KEY),
    ).rejects.toMatchObject({
      code: "INTERNAL",
    });
  });

  it("canonicalizes nested JSON independent of object insertion order", () => {
    expect(canonicalJson({ z: 1, a: { y: 2, x: [3, 4] } })).toBe(
      '{"a":{"x":[3,4],"y":2},"z":1}',
    );
  });

  it("hashes and forwards a chunked stream without changing its bytes", async () => {
    const chunks = [
      new TextEncoder().encode("scaling "),
      new TextEncoder().encode("neuro "),
      new TextEncoder().encode("stream"),
    ];
    const source = new ReadableStream<Uint8Array>({
      start(controller) {
        for (const chunk of chunks) controller.enqueue(chunk);
        controller.close();
      },
    });
    const hashed = sha256PassThrough(source);
    const forwarded = new Uint8Array(await new Response(hashed.body).arrayBuffer());
    const expected = new TextEncoder().encode("scaling neuro stream");
    expect(forwarded).toEqual(expected);
    expect(await hashed.sha256).toBe(await sha256Hex(expected));
  });

  it(
    "keeps a larger-than-Worker-memory verification stream backpressured",
    async () => {
      const chunkBytes = 1024 * 1024;
      const chunkCount = 144;
      let emitted = 0;
      let consumed = 0;
      let maximumQueuedChunks = 0;
      const source = new ReadableStream<Uint8Array>({
        pull(controller) {
          if (emitted === chunkCount) {
            controller.close();
            return;
          }
          // Use a distinct allocation for every chunk so an implementation
          // that drains the source ahead of a slow validator really would
          // retain more than the Worker's 128 MiB memory allowance.
          const chunk = new Uint8Array(chunkBytes);
          chunk[0] = emitted % 251;
          chunk[chunk.length - 1] = (emitted * 7) % 251;
          controller.enqueue(chunk);
          emitted += 1;
          maximumQueuedChunks = Math.max(
            maximumQueuedChunks,
            emitted - consumed,
          );
        },
      });
      const hashed = sha256PassThrough(source);
      const deliberatelySlowValidator = hashed.body.pipeThrough(
        new TransformStream<Uint8Array, Uint8Array>({
          async transform(chunk, controller) {
            await new Promise((resolve) => setTimeout(resolve, 1));
            consumed += 1;
            controller.enqueue(chunk);
          },
        }),
      );
      const [sourceHash, forwardedHash] = await Promise.all([
        hashed.sha256,
        sha256StreamHex(deliberatelySlowValidator),
      ]);
      expect(emitted).toBe(chunkCount);
      expect(consumed).toBe(chunkCount);
      expect(maximumQueuedChunks).toBeLessThanOrEqual(4);
      expect(forwardedHash).toBe(sourceHash);
    },
    30_000,
  );

  it("encrypts registration email with registration-bound authenticated encryption", async () => {
    const ciphertext = await encryptRegistrationEmail(
      "researcher@example.edu",
      "registration-a",
      ENCRYPTION_KEY,
    );
    expect(ciphertext).not.toContain("researcher@example.edu");
    expect(
      await decryptRegistrationEmail(
        ciphertext,
        "registration-a",
        ENCRYPTION_KEY,
      ),
    ).toBe("researcher@example.edu");
    await expect(
      decryptRegistrationEmail(ciphertext, "registration-b", ENCRYPTION_KEY),
    ).rejects.toMatchObject({ code: "INTERNAL" });
  });
});
