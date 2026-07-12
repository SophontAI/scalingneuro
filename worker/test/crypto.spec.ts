import { describe, expect, it } from "vitest";
import {
  canonicalJson,
  decryptSiteKey,
  encryptSiteKey,
  randomBytes,
  sha256Hex,
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
});
