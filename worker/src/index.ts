import {
  createArchiveAccess,
  listArchive,
  parseArchiveAccessRequest,
  signArchiveDownload,
} from "./archive-access";
import {
  checkpointDicomUpload,
  completeDicomUpload,
  createDicomUpload,
  createDicomUploadPartUrl,
  getDicomUploadStatus,
  refreshDicomUploadCredentials,
} from "./dicom";
import { AppError } from "./errors";
import type { Env } from "./env";
import {
  acceptPublicContributionPolicy,
  health,
  publicContributionInfo,
  registerContributor,
} from "./service";
import {
  parseCompleteUploadRequest,
  parseCreateDicomUploadRequest,
  parseJsonText,
  parsePublicPolicyAcceptanceRequest,
  parsePublicRegistrationRequest,
  parseSignPartRequest,
} from "./validation";

const UUID =
  "([0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12})";
const SERIES_ARCHIVE_ID = "([a-f0-9]{24})";
const dicomCredentialsRoute = new RegExp(
  `^/v1/dicom-uploads/${UUID}/credentials$`,
  "u",
);
const dicomCompleteRoute = new RegExp(
  `^/v1/dicom-uploads/${UUID}/complete$`,
  "u",
);
const dicomCheckpointRoute = new RegExp(
  `^/v1/dicom-uploads/${UUID}/checkpoint$`,
  "u",
);
const dicomPartRoute = new RegExp(
  `^/v1/dicom-uploads/${UUID}/parts$`,
  "u",
);
const dicomStatusRoute = new RegExp(`^/v1/dicom-uploads/${UUID}$`, "u");
const archiveDownloadRoute = new RegExp(
  `^/v1/archive/${UUID}/${SERIES_ARCHIVE_ID}/download$`,
  "u",
);

function apiHeaders(requestId: string): Headers {
  return new Headers({
    "cache-control": "no-store",
    "content-type": "application/json; charset=utf-8",
    "referrer-policy": "no-referrer",
    "x-content-type-options": "nosniff",
    "x-request-id": requestId,
  });
}

function json(body: unknown, requestId: string, status = 200): Response {
  return new Response(`${JSON.stringify(body)}\n`, {
    status,
    headers: apiHeaders(requestId),
  });
}

function redirect(url: string, requestId: string): Response {
  return new Response(null, {
    status: 302,
    headers: {
      "cache-control": "no-store",
      location: url,
      "referrer-policy": "no-referrer",
      "x-content-type-options": "nosniff",
      "x-request-id": requestId,
    },
  });
}

async function requestJson(request: Request): Promise<unknown> {
  const maximumBytes = 1024 * 1024;
  const contentType = request.headers
    .get("content-type")
    ?.split(";", 1)[0]
    ?.trim()
    .toLowerCase();
  if (contentType !== "application/json") {
    throw new AppError(
      "INVALID_REQUEST",
      415,
      "Content-Type must be application/json",
    );
  }
  const contentLength = request.headers.get("content-length");
  if (contentLength && Number(contentLength) > maximumBytes) {
    throw new AppError("INVALID_REQUEST", 413, "Request body exceeds 1 MiB");
  }
  if (!request.body) return parseJsonText("");

  const reader = request.body.getReader();
  const chunks: Uint8Array[] = [];
  let total = 0;
  while (true) {
    const { done, value } = await reader.read();
    if (done) break;
    if (!value) continue;
    total += value.byteLength;
    if (total > maximumBytes) {
      await reader.cancel();
      throw new AppError("INVALID_REQUEST", 413, "Request body exceeds 1 MiB");
    }
    chunks.push(value);
  }
  const bytes = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, offset);
    offset += chunk.byteLength;
  }
  try {
    return parseJsonText(
      new TextDecoder("utf-8", { fatal: true }).decode(bytes),
    );
  } catch (error) {
    if (error instanceof AppError) throw error;
    throw new AppError("INVALID_REQUEST", 400, "Request body must be UTF-8");
  }
}

function idMatch(regex: RegExp, pathname: string): string | null {
  return regex.exec(pathname)?.[1] ?? null;
}

function routeLabel(pathname: string): string {
  if (
    [
      "/health",
      "/v1/contribution",
      "/v1/device/policy",
      "/v1/register",
      "/v1/dicom-uploads",
      "/v1/archive-access",
      "/v1/archive",
    ].includes(pathname)
  ) {
    return pathname;
  }
  if (dicomCredentialsRoute.test(pathname)) {
    return "/v1/dicom-uploads/:id/credentials";
  }
  if (dicomCompleteRoute.test(pathname)) {
    return "/v1/dicom-uploads/:id/complete";
  }
  if (dicomCheckpointRoute.test(pathname)) {
    return "/v1/dicom-uploads/:id/checkpoint";
  }
  if (dicomPartRoute.test(pathname)) return "/v1/dicom-uploads/:id/parts";
  if (dicomStatusRoute.test(pathname)) return "/v1/dicom-uploads/:id";
  if (archiveDownloadRoute.test(pathname)) {
    return "/v1/archive/:upload/:series/download";
  }
  return "unknown";
}

export async function fetchHandler(
  request: Request,
  env: Env,
  _ctx?: ExecutionContext,
): Promise<Response> {
  const requestId = crypto.randomUUID();
  const startedAt = Date.now();
  const path = new URL(request.url).pathname;
  try {
    if (request.method === "GET" && path === "/health") {
      return json(await health(env), requestId);
    }
    if (request.method === "GET" && path === "/v1/contribution") {
      return json(
        publicContributionInfo(request.headers.get("user-agent")),
        requestId,
      );
    }
    if (request.method === "POST" && path === "/v1/device/policy") {
      return json(
        await acceptPublicContributionPolicy(
          request,
          env,
          parsePublicPolicyAcceptanceRequest(await requestJson(request)),
        ),
        requestId,
      );
    }
    if (request.method === "POST" && path === "/v1/register") {
      return json(
        await registerContributor(
          env,
          parsePublicRegistrationRequest(await requestJson(request)),
        ),
        requestId,
        201,
      );
    }
    if (request.method === "POST" && path === "/v1/dicom-uploads") {
      const result = await createDicomUpload(
        request,
        env,
        parseCreateDicomUploadRequest(await requestJson(request)),
      );
      return json(result.body, requestId, result.created ? 201 : 200);
    }
    if (request.method === "POST" && path === "/v1/archive-access") {
      return json(
        await createArchiveAccess(
          env,
          parseArchiveAccessRequest(await requestJson(request)),
        ),
        requestId,
        201,
      );
    }
    if (request.method === "GET" && path === "/v1/archive") {
      return json(await listArchive(request, env), requestId);
    }

    const credentialsUploadId = idMatch(dicomCredentialsRoute, path);
    if (request.method === "POST" && credentialsUploadId) {
      return json(
        await refreshDicomUploadCredentials(
          request,
          env,
          credentialsUploadId,
        ),
        requestId,
      );
    }
    const completeUploadId = idMatch(dicomCompleteRoute, path);
    if (request.method === "POST" && completeUploadId) {
      return json(
        await completeDicomUpload(
          request,
          env,
          completeUploadId,
          parseCompleteUploadRequest(await requestJson(request)),
        ),
        requestId,
      );
    }
    const checkpointUploadId = idMatch(dicomCheckpointRoute, path);
    if (request.method === "POST" && checkpointUploadId) {
      return json(
        await checkpointDicomUpload(
          request,
          env,
          checkpointUploadId,
          parseCompleteUploadRequest(await requestJson(request)),
        ),
        requestId,
      );
    }
    const partUploadId = idMatch(dicomPartRoute, path);
    if (request.method === "POST" && partUploadId) {
      return json(
        await createDicomUploadPartUrl(
          request,
          env,
          partUploadId,
          parseSignPartRequest(await requestJson(request)),
        ),
        requestId,
      );
    }
    const statusUploadId = idMatch(dicomStatusRoute, path);
    if (request.method === "GET" && statusUploadId) {
      return json(
        await getDicomUploadStatus(request, env, statusUploadId),
        requestId,
      );
    }
    const archiveMatch = archiveDownloadRoute.exec(path);
    if (request.method === "GET" && archiveMatch?.[1] && archiveMatch[2]) {
      return redirect(
        await signArchiveDownload(
          request,
          env,
          archiveMatch[1],
          archiveMatch[2],
        ),
        requestId,
      );
    }
    throw new AppError("NOT_FOUND", 404, "Route was not found");
  } catch (error) {
    const appError =
      error instanceof AppError
        ? error
        : new AppError("INTERNAL", 500, "Unexpected service error");
    const errorBody: Record<string, unknown> = {
      code: appError.code,
      message: appError.message,
      request_id: requestId,
    };
    if (appError.details) errorBody.details = appError.details;
    const log = {
      event: "api_error",
      request_id: requestId,
      route: routeLabel(path),
      method: request.method,
      status: appError.status,
      code: appError.code,
      duration_ms: Date.now() - startedAt,
    };
    if (appError.status >= 500) console.error(JSON.stringify(log));
    else console.warn(JSON.stringify(log));
    return json({ error: errorBody }, requestId, appError.status);
  }
}

export default {
  fetch: fetchHandler,
} satisfies ExportedHandler<Env>;
