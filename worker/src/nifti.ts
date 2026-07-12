export interface NiftiFacts {
  dimensions: number[];
  voxel_size_mm: number[];
  datatype: string;
  bits_per_voxel: number;
  affine: number[][];
  orientation: string;
  volume_count: number;
  tr_seconds: number;
  uncompressed_sha256: string;
  uncompressed_size: number;
}

export interface SidecarImageFacts {
  dimensions: number[];
  voxel_size_mm: number[];
  datatype: string;
  bits_per_voxel: number;
  affine: number[][];
  orientation: string;
  volume_count: number;
  tr_seconds: number;
}

type Endian = "little" | "big";

const HEADER_BYTES = 352;
const MAX_UNCOMPRESSED_BYTES = 64 * 1024 ** 3;

function digestStream(): DigestStream {
  const workersCrypto = crypto as Crypto & {
    DigestStream: typeof DigestStream;
  };
  return new workersCrypto.DigestStream("SHA-256");
}

function hex(bytes: Uint8Array): string {
  return [...bytes]
    .map((value) => value.toString(16).padStart(2, "0"))
    .join("");
}

async function firstBytes(
  stream: ReadableStream<Uint8Array>,
  length: number,
): Promise<Uint8Array> {
  const reader = stream.getReader();
  const output = new Uint8Array(length);
  let offset = 0;
  try {
    while (offset < length) {
      const { done, value } = await reader.read();
      if (done || !value) throw new Error("NIfTI is shorter than its header");
      const take = Math.min(value.byteLength, length - offset);
      output.set(value.subarray(0, take), offset);
      offset += take;
    }
  } finally {
    // Do not await cancellation here: on a tee'd stream that promise may wait
    // for the digest branch to drain, while the digest intentionally starts
    // only after this header has passed the decompression-bomb safety checks.
    void reader.cancel().catch(() => undefined);
  }
  return output;
}

function i16(bytes: Uint8Array, offset: number, endian: Endian): number {
  return new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength).getInt16(
    offset,
    endian === "little",
  );
}

function i32(bytes: Uint8Array, offset: number, endian: Endian): number {
  return new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength).getInt32(
    offset,
    endian === "little",
  );
}

function f32(bytes: Uint8Array, offset: number, endian: Endian): number {
  return new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength).getFloat32(
    offset,
    endian === "little",
  );
}

function datatypeName(code: number): string {
  const names: Record<number, string> = {
    2: "uint8",
    4: "int16",
    8: "int32",
    16: "float32",
    64: "float64",
    256: "int8",
    512: "uint16",
    768: "uint32",
    1024: "int64",
    1280: "uint64",
  };
  const name = names[code];
  if (!name) throw new Error("Unsupported NIfTI datatype");
  return name;
}

function expectedBitDepth(code: number): number {
  if (code === 2 || code === 256) return 8;
  if (code === 4 || code === 512) return 16;
  if (code === 8 || code === 16 || code === 768) return 32;
  if (code === 64 || code === 1024 || code === 1280) return 64;
  throw new Error("Unsupported NIfTI datatype");
}

function affineFromHeader(
  header: Uint8Array,
  pixdim: number[],
  endian: Endian,
): number[][] {
  if (i16(header, 254, endian) > 0) {
    const affine = Array.from({ length: 4 }, () => [0, 0, 0, 0]);
    for (let row = 0; row < 3; row += 1) {
      for (let column = 0; column < 4; column += 1) {
        affine[row]![column] = f32(
          header,
          280 + (row * 4 + column) * 4,
          endian,
        );
      }
    }
    affine[3]![3] = 1;
    return affine;
  }
  if (i16(header, 252, endian) <= 0) {
    throw new Error("NIfTI lacks qform and sform geometry");
  }

  const b = f32(header, 256, endian);
  const c = f32(header, 260, endian);
  const d = f32(header, 264, endian);
  const aSquared = 1 - b * b - c * c - d * d;
  const a = aSquared > 1e-7 ? Math.sqrt(aSquared) : 0;
  const rotation = [
    [a * a + b * b - c * c - d * d, 2 * (b * c - a * d), 2 * (b * d + a * c)],
    [2 * (b * c + a * d), a * a + c * c - b * b - d * d, 2 * (c * d - a * b)],
    [2 * (b * d - a * c), 2 * (c * d + a * b), a * a + d * d - c * c - b * b],
  ];
  const scales = [
    Math.abs(pixdim[1]!),
    Math.abs(pixdim[2]!),
    Math.abs(pixdim[3]!) * (pixdim[0]! < 0 ? -1 : 1),
  ];
  const offsets = [
    f32(header, 268, endian),
    f32(header, 272, endian),
    f32(header, 276, endian),
  ];
  const affine = Array.from({ length: 4 }, () => [0, 0, 0, 0]);
  for (let row = 0; row < 3; row += 1) {
    for (let column = 0; column < 3; column += 1) {
      affine[row]![column] = rotation[row]![column]! * scales[column]!;
    }
    affine[row]![3] = offsets[row]!;
  }
  affine[3]![3] = 1;
  return affine;
}

function orientation(affine: number[][]): string {
  const used = [false, false, false];
  let result = "";
  for (let column = 0; column < 3; column += 1) {
    let selected = -1;
    let selectedValue = 0;
    for (let axis = 0; axis < 3; axis += 1) {
      const value = affine[axis]![column]!;
      if (!used[axis] && Math.abs(value) > Math.abs(selectedValue)) {
        selected = axis;
        selectedValue = value;
      }
    }
    if (selected < 0 || Math.abs(selectedValue) < 1e-8) {
      throw new Error("NIfTI orientation is degenerate");
    }
    used[selected] = true;
    result += [
      selectedValue >= 0 ? "R" : "L",
      selectedValue >= 0 ? "A" : "P",
      selectedValue >= 0 ? "S" : "I",
    ][selected];
  }
  return result;
}

function parseHeader(header: Uint8Array): Omit<
  NiftiFacts,
  "uncompressed_sha256" | "uncompressed_size"
> & { expected_size: number } {
  const endian: Endian =
    i32(header, 0, "little") === 348
      ? "little"
      : i32(header, 0, "big") === 348
        ? "big"
        : (() => {
            throw new Error("Invalid NIfTI-1 header size");
          })();
  if (
    header[344] !== 0x6e ||
    header[345] !== 0x2b ||
    header[346] !== 0x31 ||
    header[347] !== 0
  ) {
    throw new Error("NIfTI single-file magic is invalid");
  }
  if (header.subarray(348, 352).some((value) => value !== 0)) {
    throw new Error("NIfTI extensions are not allowed");
  }
  for (const [start, end] of [
    [4, 32],
    [148, 252],
    [328, 344],
  ] as const) {
    if (header.subarray(start, end).some((value) => value !== 0)) {
      throw new Error("NIfTI text header fields are not sanitized");
    }
  }
  if (i16(header, 40, endian) !== 4) {
    throw new Error("Only four-dimensional functional NIfTI is accepted");
  }
  const dimensions = [1, 2, 3, 4].map((index) =>
    i16(header, 40 + index * 2, endian),
  );
  if (
    dimensions.slice(0, 3).some((value) => value < 8 || value > 4096) ||
    dimensions[3]! < 10 ||
    dimensions[3]! > 10_000_000
  ) {
    throw new Error("NIfTI dimensions are outside the functional EPI contract");
  }
  const pixdim = Array.from({ length: 8 }, (_, index) =>
    f32(header, 76 + index * 4, endian),
  );
  const voxelSize = pixdim.slice(1, 4).map(Math.abs);
  if (voxelSize.some((value) => !Number.isFinite(value) || value <= 0 || value > 100)) {
    throw new Error("NIfTI voxel sizes are invalid");
  }
  const datatypeCode = i16(header, 70, endian);
  const datatype = datatypeName(datatypeCode);
  const bitsPerVoxel = i16(header, 72, endian);
  if (bitsPerVoxel !== expectedBitDepth(datatypeCode)) {
    throw new Error("NIfTI datatype and bit depth disagree");
  }
  const voxelOffset = f32(header, 108, endian);
  if (
    !Number.isFinite(voxelOffset) ||
    !Number.isInteger(voxelOffset) ||
    voxelOffset < HEADER_BYTES
  ) {
    throw new Error("NIfTI voxel offset is invalid");
  }
  const scaleSlope = f32(header, 112, endian);
  const scaleIntercept = f32(header, 116, endian);
  if (!Number.isFinite(scaleSlope) || !Number.isFinite(scaleIntercept)) {
    throw new Error("NIfTI intensity scaling is non-finite");
  }
  const expectedSize =
    Math.round(voxelOffset) +
    dimensions.reduce((product, value) => product * value, 1) *
      (bitsPerVoxel / 8);
  if (!Number.isSafeInteger(expectedSize) || expectedSize > MAX_UNCOMPRESSED_BYTES) {
    throw new Error("NIfTI uncompressed size exceeds the v1 safety limit");
  }
  const affine = affineFromHeader(header, pixdim, endian);
  if (affine.flat().some((value) => !Number.isFinite(value))) {
    throw new Error("NIfTI affine contains non-finite values");
  }
  const determinant =
    affine[0]![0]! *
      (affine[1]![1]! * affine[2]![2]! - affine[1]![2]! * affine[2]![1]!) -
    affine[0]![1]! *
      (affine[1]![0]! * affine[2]![2]! - affine[1]![2]! * affine[2]![0]!) +
    affine[0]![2]! *
      (affine[1]![0]! * affine[2]![1]! - affine[1]![1]! * affine[2]![0]!);
  if (!Number.isFinite(determinant) || Math.abs(determinant) <= 1e-8) {
    throw new Error("NIfTI affine is degenerate");
  }
  const spatialUnits = header[123]! & 0x07;
  const temporalUnits = header[123]! & 0x38;
  if (spatialUnits !== 2 || ![8, 16, 24].includes(temporalUnits)) {
    throw new Error("NIfTI spatial or temporal units are invalid");
  }
  const rawTr = Math.abs(pixdim[4]!);
  const trSeconds =
    temporalUnits === 16
      ? rawTr / 1_000
      : temporalUnits === 24
        ? rawTr / 1_000_000
        : rawTr;
  if (!Number.isFinite(trSeconds) || trSeconds < 0.1 || trSeconds > 20) {
    throw new Error("NIfTI repetition time is invalid");
  }
  return {
    dimensions,
    voxel_size_mm: voxelSize,
    datatype,
    bits_per_voxel: bitsPerVoxel,
    affine,
    orientation: orientation(affine),
    volume_count: dimensions[3]!,
    tr_seconds: trSeconds,
    expected_size: expectedSize,
  };
}

export async function inspectGzipNifti(
  body: ReadableStream<Uint8Array>,
  expectedUncompressedSha256: string,
): Promise<NiftiFacts> {
  const decompressor = new DecompressionStream("gzip") as unknown as {
    readable: ReadableStream<Uint8Array>;
    writable: WritableStream<Uint8Array>;
  };
  const decompressed = body.pipeThrough(decompressor);
  const [digestBody, headerBody] = decompressed.tee();
  const parsed = parseHeader(await firstBytes(headerBody, HEADER_BYTES));
  const digest = digestStream();
  let observedBytes = 0;
  const boundedBody = digestBody.pipeThrough(
    new TransformStream<Uint8Array, Uint8Array>({
      transform(chunk, controller) {
        observedBytes += chunk.byteLength;
        if (observedBytes > parsed.expected_size) {
          throw new Error("NIfTI expands beyond its declared voxel payload");
        }
        controller.enqueue(chunk);
      },
    }),
  );
  await boundedBody.pipeTo(digest);
  const uncompressedSize = Number(digest.bytesWritten);
  const uncompressedSha256 = hex(new Uint8Array(await digest.digest));
  if (
    uncompressedSize !== parsed.expected_size ||
    uncompressedSha256 !== expectedUncompressedSha256
  ) {
    throw new Error("NIfTI uncompressed size or checksum does not match");
  }
  const { expected_size: _expectedSize, ...facts } = parsed;
  return {
    ...facts,
    uncompressed_sha256: uncompressedSha256,
    uncompressed_size: uncompressedSize,
  };
}

function close(left: number, right: number): boolean {
  return Math.abs(left - right) <= 1e-5 * Math.max(1, Math.abs(left), Math.abs(right));
}

export function assertNiftiMatchesSidecar(
  nifti: NiftiFacts,
  image: SidecarImageFacts,
): void {
  if (
    nifti.dimensions.length !== image.dimensions.length ||
    nifti.dimensions.some((value, index) => value !== image.dimensions[index]) ||
    nifti.voxel_size_mm.some(
      (value, index) => !close(value, image.voxel_size_mm[index]!),
    ) ||
    nifti.datatype !== image.datatype ||
    nifti.bits_per_voxel !== image.bits_per_voxel ||
    nifti.orientation !== image.orientation ||
    nifti.volume_count !== image.volume_count ||
    !close(nifti.tr_seconds, image.tr_seconds) ||
    nifti.affine.some((row, rowIndex) =>
      row.some((value, columnIndex) =>
        !close(value, image.affine[rowIndex]![columnIndex]!),
      ),
    )
  ) {
    throw new Error("NIfTI header does not match the metadata sidecar");
  }
}
