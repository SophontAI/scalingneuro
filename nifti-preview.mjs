const NIFTI_HEADER_BYTES = 352;
const DEFAULT_MAX_FIRST_VOLUME_BYTES = 512 * 1024 * 1024;

function niftiEndian(view) {
  if (view.getInt32(0, true) === 348) return true;
  if (view.getInt32(0, false) === 348) return false;
  throw new Error('NIfTI-1 header marker was not found');
}

export function firstVolumeByteLength(header, maxBytes = DEFAULT_MAX_FIRST_VOLUME_BYTES) {
  if (header.byteLength < NIFTI_HEADER_BYTES) {
    throw new Error('file is too small to contain a NIfTI-1 volume');
  }

  const view = new DataView(header.buffer, header.byteOffset, header.byteLength);
  const littleEndian = niftiEndian(view);
  const ndim = view.getInt16(40, littleEndian);
  if (ndim < 3 || ndim > 7) throw new Error('viewer requires a valid 3D NIfTI volume');

  const dims = [42, 44, 46].map((offset) => view.getInt16(offset, littleEndian));
  if (dims.some((dimension) => !Number.isInteger(dimension) || dimension < 2)) {
    throw new Error('invalid NIfTI dimensions');
  }

  const bitsPerVoxel = view.getInt16(72, littleEndian);
  if (bitsPerVoxel < 8 || bitsPerVoxel % 8 !== 0) {
    throw new Error('invalid NIfTI voxel bit depth');
  }

  const voxelOffsetValue = view.getFloat32(108, littleEndian);
  const voxelOffset = Math.floor(voxelOffsetValue);
  if (!Number.isFinite(voxelOffsetValue) || voxelOffset < 348) {
    throw new Error('invalid NIfTI voxel offset');
  }

  const voxelCount = dims.reduce((product, dimension) => product * dimension, 1);
  const requiredBytes = voxelOffset + voxelCount * (bitsPerVoxel / 8);
  if (!Number.isSafeInteger(requiredBytes) || requiredBytes < NIFTI_HEADER_BYTES) {
    throw new Error('invalid NIfTI first-volume size');
  }
  if (requiredBytes > maxBytes) {
    throw new Error('NIfTI first volume is too large for an in-browser preview');
  }
  return requiredBytes;
}

async function sniffStream(stream) {
  const reader = stream.getReader();
  const initialChunks = [];
  let initialBytes = 0;

  while (initialBytes < 2) {
    const { value, done } = await reader.read();
    if (done) break;
    if (!value?.byteLength) continue;
    initialChunks.push(value);
    initialBytes += value.byteLength;
  }

  const first = initialChunks[0]?.[0];
  const second = initialChunks[0]?.byteLength > 1
    ? initialChunks[0][1]
    : initialChunks[1]?.[0];
  let replayIndex = 0;
  const replayed = new ReadableStream({
    async pull(controller) {
      if (replayIndex < initialChunks.length) {
        controller.enqueue(initialChunks[replayIndex++]);
        return;
      }
      const { value, done } = await reader.read();
      if (done) controller.close();
      else controller.enqueue(value);
    },
    cancel(reason) {
      return reader.cancel(reason);
    },
  });

  return { gzipped: first === 0x1f && second === 0x8b, stream: replayed };
}

export async function readFirstNiftiVolume(stream, options = {}) {
  const maxBytes = options.maxBytes ?? DEFAULT_MAX_FIRST_VOLUME_BYTES;
  const reader = stream.getReader();
  const header = new Uint8Array(NIFTI_HEADER_BYTES);
  let headerBytes = 0;
  let overflow;

  while (headerBytes < NIFTI_HEADER_BYTES) {
    const { value, done } = await reader.read();
    if (done) throw new Error('NIfTI stream ended before its header was complete');
    if (!value?.byteLength) continue;
    const take = Math.min(value.byteLength, NIFTI_HEADER_BYTES - headerBytes);
    header.set(value.subarray(0, take), headerBytes);
    headerBytes += take;
    if (take < value.byteLength) overflow = value.subarray(take);
  }

  const requiredBytes = firstVolumeByteLength(header, maxBytes);
  const firstVolume = new Uint8Array(requiredBytes);
  firstVolume.set(header);
  let written = NIFTI_HEADER_BYTES;

  if (overflow?.byteLength && written < requiredBytes) {
    const take = Math.min(overflow.byteLength, requiredBytes - written);
    firstVolume.set(overflow.subarray(0, take), written);
    written += take;
  }

  while (written < requiredBytes) {
    const { value, done } = await reader.read();
    if (done) throw new Error('NIfTI stream ended before its first 3D volume was complete');
    if (!value?.byteLength) continue;
    const take = Math.min(value.byteLength, requiredBytes - written);
    firstVolume.set(value.subarray(0, take), written);
    written += take;
  }

  if (options.cancel !== false) {
    try {
      await reader.cancel('First NIfTI volume is complete');
    } catch {
      // The preview is already complete; a late transport cancellation failure
      // must not discard it.
    }
  } else {
    reader.releaseLock();
  }
  return firstVolume.buffer;
}

async function readFirstGzippedNiftiVolume(stream, options = {}) {
  const sourceReader = stream.getReader();
  const decompressor = new DecompressionStream('gzip');
  const writer = decompressor.writable.getWriter();
  let settled = false;

  const outcomePromise = readFirstNiftiVolume(decompressor.readable, {
    maxBytes: options.maxBytes,
    cancel: false,
  }).then(
    (value) => {
      settled = true;
      return { value };
    },
    (error) => {
      settled = true;
      return { error };
    },
  );

  let pumpError;
  try {
    while (!settled) {
      const { value, done } = await sourceReader.read();
      if (done) {
        await writer.close();
        break;
      }
      if (!value?.byteLength) continue;
      await writer.write(value);
      // Let the output reader consume this chunk before requesting another.
      // DecompressionStream otherwise greedily drains a fetch response even
      // after the preview has enough decompressed bytes.
      await Promise.resolve();
    }
  } catch (error) {
    pumpError = error;
  }

  if (settled || pumpError) {
    await Promise.allSettled([
      sourceReader.cancel('First NIfTI volume is complete'),
      writer.abort(pumpError ?? 'First NIfTI volume is complete'),
    ]);
  }

  const outcome = await outcomePromise;
  if (outcome.error) throw outcome.error;
  if (pumpError) throw pumpError;
  return outcome.value;
}

export async function fetchFirstNiftiVolume(url, options = {}) {
  const fetchImpl = options.fetchImpl ?? fetch;
  const response = await fetchImpl(url, {
    cache: 'no-store',
    signal: options.signal,
  });
  if (!response.ok) throw new Error(`NIfTI source returned ${response.status}`);
  if (!response.body) throw new Error('this browser cannot stream the NIfTI response');

  const sniffed = await sniffStream(response.body);
  let decoded = sniffed.stream;
  if (sniffed.gzipped) {
    if (typeof DecompressionStream === 'undefined') {
      throw new Error('this browser cannot decompress .nii.gz files');
    }
    return readFirstGzippedNiftiVolume(decoded, { maxBytes: options.maxBytes });
  }

  return readFirstNiftiVolume(decoded, { maxBytes: options.maxBytes });
}
