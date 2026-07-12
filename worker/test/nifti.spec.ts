import { describe, expect, it } from "vitest";
import { sha256Hex } from "../src/crypto";
import { assertNiftiMatchesSidecar, inspectGzipNifti } from "../src/nifti";

async function fixture(
  mutate?: (bytes: Uint8Array<ArrayBuffer>, view: DataView<ArrayBuffer>) => void,
): Promise<{
  gzip: Uint8Array<ArrayBuffer>;
  uncompressedSha256: string;
}> {
  const dimensions = [64, 64, 16, 10];
  const bytes = new Uint8Array(
    352 + dimensions.reduce((product, value) => product * value, 1) * 2,
  );
  const view = new DataView(bytes.buffer);
  view.setInt32(0, 348, true);
  view.setInt16(40, 4, true);
  dimensions.forEach((value, index) =>
    view.setInt16(42 + index * 2, value, true),
  );
  view.setInt16(70, 4, true);
  view.setInt16(72, 16, true);
  view.setFloat32(80, 2, true);
  view.setFloat32(84, 2, true);
  view.setFloat32(88, 2, true);
  view.setFloat32(92, 0.8, true);
  view.setFloat32(108, 352, true);
  bytes[123] = 10;
  view.setInt16(254, 1, true);
  view.setFloat32(280, 2, true);
  view.setFloat32(300, 2, true);
  view.setFloat32(320, 2, true);
  bytes.set([0x6e, 0x2b, 0x31, 0], 344);
  // Force many decompressed chunks instead of a trivially compressible all-zero body.
  for (let index = 352; index < bytes.length; index += 1) {
    bytes[index] = (index * 31 + (index >>> 8)) & 0xff;
  }
  mutate?.(bytes, view);
  const body = new Response(bytes).body;
  if (!body) throw new Error("fixture stream unavailable");
  const gzip = new Uint8Array(
    await new Response(body.pipeThrough(new CompressionStream("gzip"))).arrayBuffer(),
  );
  return { gzip, uncompressedSha256: await sha256Hex(bytes) };
}

describe("server-side NIfTI verification", () => {
  it("streams a multi-chunk gzip and verifies its uncompressed scientific hash", async () => {
    const value = await fixture();
    const body = new Response(value.gzip).body;
    if (!body) throw new Error("fixture stream unavailable");
    const facts = await inspectGzipNifti(body, value.uncompressedSha256);
    expect(facts).toMatchObject({
      dimensions: [64, 64, 16, 10],
      voxel_size_mm: [2, 2, 2],
      datatype: "int16",
      bits_per_voxel: 16,
      orientation: "RAS",
      volume_count: 10,
      tr_seconds: expect.closeTo(0.8),
      uncompressed_sha256: value.uncompressedSha256,
    });
    expect(() => assertNiftiMatchesSidecar(facts, facts)).not.toThrow();
  }, 10_000);

  it("rejects a false uncompressed checksum", async () => {
    const value = await fixture();
    const body = new Response(value.gzip).body;
    if (!body) throw new Error("fixture stream unavailable");
    await expect(inspectGzipNifti(body, "0".repeat(64))).rejects.toThrow(
      /checksum/u,
    );
  });

  it("rejects invalid units and non-finite intensity scaling", async () => {
    for (const mutate of [
      (bytes: Uint8Array<ArrayBuffer>) => {
        bytes[123] = 8;
      },
      (_bytes: Uint8Array<ArrayBuffer>, view: DataView<ArrayBuffer>) => {
        view.setFloat32(112, Number.NaN, true);
      },
    ]) {
      const value = await fixture(mutate);
      const body = new Response(value.gzip).body;
      if (!body) throw new Error("fixture stream unavailable");
      await expect(
        inspectGzipNifti(body, value.uncompressedSha256),
      ).rejects.toThrow(/units|scaling/u);
    }
  });
});
