import assert from 'node:assert/strict';
import { gzipSync } from 'node:zlib';
import test from 'node:test';

import {
  fetchFirstNiftiVolume,
  firstVolumeByteLength,
  readFirstNiftiVolume,
} from '../nifti-preview.mjs';

function niftiFixture({ dims = [16, 16, 4, 2000], gzipped = true } = {}) {
  const voxelOffset = 352;
  const voxels = dims.reduce((product, dimension) => product * dimension, 1);
  const bytes = new Uint8Array(voxelOffset + voxels * 2);
  const view = new DataView(bytes.buffer);
  view.setInt32(0, 348, true);
  view.setInt16(40, 4, true);
  dims.forEach((dimension, index) => view.setInt16(42 + index * 2, dimension, true));
  view.setInt16(70, 4, true);
  view.setInt16(72, 16, true);
  view.setFloat32(108, voxelOffset, true);
  bytes.set([0x6e, 0x2b, 0x31, 0], 344);

  let state = 0x12345678;
  for (let offset = voxelOffset; offset < bytes.byteLength; offset += 2) {
    state = (Math.imul(state, 1664525) + 1013904223) >>> 0;
    view.setInt16(offset, state & 0x7fff, true);
  }

  const firstVolumeBytes = voxelOffset + dims[0] * dims[1] * dims[2] * 2;
  return {
    bytes,
    encoded: gzipped ? new Uint8Array(gzipSync(bytes)) : bytes,
    firstVolumeBytes,
  };
}

function trackedResponse(bytes, chunkSize = 8192) {
  let offset = 0;
  let cancelled = false;
  const body = new ReadableStream({
    pull(controller) {
      if (offset >= bytes.byteLength) {
        controller.close();
        return;
      }
      const end = Math.min(offset + chunkSize, bytes.byteLength);
      controller.enqueue(bytes.slice(offset, end));
      offset = end;
    },
    cancel() {
      cancelled = true;
    },
  });
  return {
    response: new Response(body, { status: 200 }),
    stats: () => ({ bytesRead: offset, cancelled }),
  };
}

test('stream-decompresses only the first 3D volume from a .nii.gz response', async () => {
  const fixture = niftiFixture();
  const tracked = trackedResponse(fixture.encoded);
  const result = new Uint8Array(await fetchFirstNiftiVolume('https://example.test/scan.nii.gz', {
    fetchImpl: async () => tracked.response,
  }));

  assert.equal(result.byteLength, fixture.firstVolumeBytes);
  assert.deepEqual(result, fixture.bytes.subarray(0, fixture.firstVolumeBytes));
  assert.ok(tracked.stats().bytesRead < fixture.encoded.byteLength);
  assert.equal(tracked.stats().cancelled, true);
});

test('stops an uncompressed .nii stream after its first volume', async () => {
  const fixture = niftiFixture({ gzipped: false });
  const tracked = trackedResponse(fixture.encoded, 257);
  const result = new Uint8Array(await fetchFirstNiftiVolume('https://example.test/scan.nii', {
    fetchImpl: async () => tracked.response,
  }));

  assert.deepEqual(result, fixture.bytes.subarray(0, fixture.firstVolumeBytes));
  assert.ok(tracked.stats().bytesRead < fixture.encoded.byteLength);
  assert.equal(tracked.stats().cancelled, true);
});

test('calculates the first-volume boundary without including later timepoints', () => {
  const fixture = niftiFixture({ dims: [8, 9, 10, 11], gzipped: false });
  assert.equal(firstVolumeByteLength(fixture.bytes.subarray(0, 352)), 352 + 8 * 9 * 10 * 2);
});

test('rejects a stream truncated before the first volume is complete', async () => {
  const fixture = niftiFixture({ dims: [8, 9, 10, 11], gzipped: false });
  const truncated = fixture.bytes.subarray(0, fixture.firstVolumeBytes - 1);
  await assert.rejects(
    readFirstNiftiVolume(new Blob([truncated]).stream()),
    /first 3D volume was complete/,
  );
});

test('fails closed when the declared preview volume exceeds the browser limit', () => {
  const fixture = niftiFixture({ dims: [8, 9, 10, 11], gzipped: false });
  assert.throws(
    () => firstVolumeByteLength(fixture.bytes.subarray(0, 352), 1000),
    /too large for an in-browser preview/,
  );
});
