import { AppError } from "./errors";
import type { Env } from "./env";
import {
  claimProcessingJob,
  checkpointDicomUpload,
  cleanupPendingRejectedDicomInputs,
  completeDicomUpload,
  completeProcessingJob,
  createDicomUpload,
  createDicomUploadPartUrl,
  failProcessingJob,
  getDicomUploadStatus,
  grantProcessingOutputs,
  heartbeatProcessingJob,
  refreshDicomUploadCredentials,
} from "./dicom";
import {
  adminCleanup,
  acceptPublicContributionPolicy,
  cleanupAbandoned,
  completeUpload,
  createAdminInvite,
  createUploadPartUrl,
  createUpload,
  enroll,
  getUploadStatus,
  health,
  listContributorRegistrations,
  publicContributionInfo,
  refreshUploadCredentials,
  registerContributor,
  revokeDevice,
  revokeInvite,
  withdrawUpload,
} from "./service";
import {
  parseAdminInviteRequest,
  parseCompleteUploadRequest,
  parseCreateDicomUploadRequest,
  parseCreateUploadRequest,
  parseEnrollRequest,
  parseJsonText,
  parsePublicRegistrationRequest,
  parsePublicPolicyAcceptanceRequest,
  parseProcessorClaimRequest,
  parseProcessorCompleteRequest,
  parseProcessorFailRequest,
  parseProcessorLeaseRequest,
  parseProcessorOutputRequest,
  parseSignPartRequest,
} from "./validation";

const UUID =
  "([0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12})";
const uploadCredentialsRoute = new RegExp(
  `^/v1/uploads/${UUID}/credentials$`,
  "u",
);
const uploadCompleteRoute = new RegExp(`^/v1/uploads/${UUID}/complete$`, "u");
const uploadPartRoute = new RegExp(`^/v1/uploads/${UUID}/parts$`, "u");
const uploadStatusRoute = new RegExp(`^/v1/uploads/${UUID}$`, "u");
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
const dicomPartRoute = new RegExp(`^/v1/dicom-uploads/${UUID}/parts$`, "u");
const dicomStatusRoute = new RegExp(`^/v1/dicom-uploads/${UUID}$`, "u");
const processorHeartbeatRoute = new RegExp(
  `^/v1/processor/jobs/${UUID}/heartbeat$`,
  "u",
);
const processorOutputsRoute = new RegExp(
  `^/v1/processor/jobs/${UUID}/outputs$`,
  "u",
);
const processorCompleteRoute = new RegExp(
  `^/v1/processor/jobs/${UUID}/complete$`,
  "u",
);
const processorFailRoute = new RegExp(
  `^/v1/processor/jobs/${UUID}/fail$`,
  "u",
);
const inviteRevokeRoute = new RegExp(`^/v1/admin/invites/${UUID}/revoke$`, "u");
const deviceRevokeRoute = new RegExp(`^/v1/admin/devices/${UUID}/revoke$`, "u");
const uploadWithdrawRoute = new RegExp(
  `^/v1/admin/uploads/${UUID}/withdraw$`,
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
  if (!request.body) {
    return parseJsonText("");
  }
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
  let body: string;
  try {
    body = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
  } catch {
    throw new AppError("INVALID_REQUEST", 400, "Request body must be UTF-8");
  }
  return parseJsonText(body);
}

function routeLabel(pathname: string): string {
  if (
    pathname === "/health" ||
    pathname === "/v1/contribution" ||
    pathname === "/v1/device/policy" ||
    pathname === "/v1/enroll" ||
    pathname === "/v1/register" ||
    pathname === "/v1/uploads" ||
    pathname === "/v1/dicom-uploads" ||
    pathname === "/v1/processor/jobs/claim" ||
    pathname === "/v1/admin/invites" ||
    pathname === "/v1/admin/registrations" ||
    pathname === "/v1/admin/cleanup"
  ) {
    return pathname;
  }
  if (uploadCredentialsRoute.test(pathname))
    return "/v1/uploads/:id/credentials";
  if (uploadCompleteRoute.test(pathname)) return "/v1/uploads/:id/complete";
  if (uploadPartRoute.test(pathname)) return "/v1/uploads/:id/parts";
  if (uploadStatusRoute.test(pathname)) return "/v1/uploads/:id";
  if (dicomCredentialsRoute.test(pathname))
    return "/v1/dicom-uploads/:id/credentials";
  if (dicomCompleteRoute.test(pathname))
    return "/v1/dicom-uploads/:id/complete";
  if (dicomCheckpointRoute.test(pathname))
    return "/v1/dicom-uploads/:id/checkpoint";
  if (dicomPartRoute.test(pathname)) return "/v1/dicom-uploads/:id/parts";
  if (dicomStatusRoute.test(pathname)) return "/v1/dicom-uploads/:id";
  if (processorHeartbeatRoute.test(pathname))
    return "/v1/processor/jobs/:id/heartbeat";
  if (processorOutputsRoute.test(pathname))
    return "/v1/processor/jobs/:id/outputs";
  if (processorCompleteRoute.test(pathname))
    return "/v1/processor/jobs/:id/complete";
  if (processorFailRoute.test(pathname))
    return "/v1/processor/jobs/:id/fail";
  if (inviteRevokeRoute.test(pathname))
    return "/v1/admin/invites/:id/revoke";
  if (deviceRevokeRoute.test(pathname))
    return "/v1/admin/devices/:id/revoke";
  if (uploadWithdrawRoute.test(pathname))
    return "/v1/admin/uploads/:id/withdraw";
  return "unknown";
}

function idMatch(regex: RegExp, pathname: string): string | null {
  return regex.exec(pathname)?.[1] ?? null;
}

export async function fetchHandler(
  request: Request,
  env: Env,
  ctx?: ExecutionContext,
): Promise<Response> {
  const requestId = crypto.randomUUID();
  const startedAt = Date.now();
  const url = new URL(request.url);
  const path = url.pathname;
  try {
    if (request.method === "GET" && path === "/health") {
      const result = await health(env);
      return json(result, requestId);
    }
    if (request.method === "GET" && path === "/v1/contribution") {
      return json(
        publicContributionInfo(request.headers.get("user-agent")),
        requestId,
      );
    }
    if (
      request.method === "POST" &&
      path === "/v1/device/policy"
    ) {
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
    if (request.method === "POST" && path === "/v1/enroll") {
      return json(
        await enroll(env, parseEnrollRequest(await requestJson(request))),
        requestId,
        201,
      );
    }
    if (request.method === "POST" && path === "/v1/uploads") {
      const result = await createUpload(
        request,
        env,
        parseCreateUploadRequest(await requestJson(request)),
      );
      return json(result.body, requestId, result.created ? 201 : 200);
    }
    if (request.method === "POST" && path === "/v1/dicom-uploads") {
      const result = await createDicomUpload(
        request,
        env,
        parseCreateDicomUploadRequest(await requestJson(request)),
      );
      return json(result.body, requestId, result.created ? 201 : 200);
    }
    if (request.method === "POST" && path === "/v1/processor/jobs/claim") {
      const result = await claimProcessingJob(
        request,
        env,
        parseProcessorClaimRequest(await requestJson(request)),
      );
      return result === null
        ? new Response(null, { status: 204, headers: apiHeaders(requestId) })
        : json(result, requestId);
    }

    const credentialsUploadId = idMatch(uploadCredentialsRoute, path);
    if (request.method === "POST" && credentialsUploadId) {
      return json(
        await refreshUploadCredentials(request, env, credentialsUploadId),
        requestId,
      );
    }
    const completeUploadId = idMatch(uploadCompleteRoute, path);
    if (request.method === "POST" && completeUploadId) {
      return json(
        await completeUpload(
          request,
          env,
          completeUploadId,
          parseCompleteUploadRequest(await requestJson(request)),
        ),
        requestId,
      );
    }
    const partUploadId = idMatch(uploadPartRoute, path);
    if (request.method === "POST" && partUploadId) {
      return json(
        await createUploadPartUrl(
          request,
          env,
          partUploadId,
          parseSignPartRequest(await requestJson(request)),
        ),
        requestId,
      );
    }
    const statusUploadId = idMatch(uploadStatusRoute, path);
    if (request.method === "GET" && statusUploadId) {
      return json(
        await getUploadStatus(request, env, statusUploadId),
        requestId,
      );
    }
    const dicomCredentialsUploadId = idMatch(dicomCredentialsRoute, path);
    if (request.method === "POST" && dicomCredentialsUploadId) {
      return json(
        await refreshDicomUploadCredentials(
          request,
          env,
          dicomCredentialsUploadId,
        ),
        requestId,
      );
    }
    const dicomCompleteUploadId = idMatch(dicomCompleteRoute, path);
    if (request.method === "POST" && dicomCompleteUploadId) {
      return json(
        await completeDicomUpload(
          request,
          env,
          dicomCompleteUploadId,
          parseCompleteUploadRequest(await requestJson(request)),
        ),
        requestId,
      );
    }
    const dicomCheckpointUploadId = idMatch(dicomCheckpointRoute, path);
    if (request.method === "POST" && dicomCheckpointUploadId) {
      return json(
        await checkpointDicomUpload(
          request,
          env,
          dicomCheckpointUploadId,
          parseCompleteUploadRequest(await requestJson(request)),
        ),
        requestId,
      );
    }
    const dicomPartUploadId = idMatch(dicomPartRoute, path);
    if (request.method === "POST" && dicomPartUploadId) {
      return json(
        await createDicomUploadPartUrl(
          request,
          env,
          dicomPartUploadId,
          parseSignPartRequest(await requestJson(request)),
        ),
        requestId,
      );
    }
    const dicomStatusUploadId = idMatch(dicomStatusRoute, path);
    if (request.method === "GET" && dicomStatusUploadId) {
      return json(
        await getDicomUploadStatus(request, env, dicomStatusUploadId),
        requestId,
      );
    }

    const heartbeatJobId = idMatch(processorHeartbeatRoute, path);
    if (request.method === "POST" && heartbeatJobId) {
      return json(
        await heartbeatProcessingJob(
          request,
          env,
          heartbeatJobId,
          parseProcessorLeaseRequest(await requestJson(request)),
        ),
        requestId,
      );
    }
    const outputsJobId = idMatch(processorOutputsRoute, path);
    if (request.method === "POST" && outputsJobId) {
      return json(
        await grantProcessingOutputs(
          request,
          env,
          outputsJobId,
          parseProcessorOutputRequest(await requestJson(request)),
        ),
        requestId,
      );
    }
    const completedJobId = idMatch(processorCompleteRoute, path);
    if (request.method === "POST" && completedJobId) {
      return json(
        await completeProcessingJob(
          request,
          env,
          completedJobId,
          parseProcessorCompleteRequest(await requestJson(request)),
        ),
        requestId,
      );
    }
    const failedJobId = idMatch(processorFailRoute, path);
    if (request.method === "POST" && failedJobId) {
      return json(
        await failProcessingJob(
          request,
          env,
          failedJobId,
          parseProcessorFailRequest(await requestJson(request)),
        ),
        requestId,
      );
    }

    if (request.method === "POST" && path === "/v1/admin/invites") {
      return json(
        await createAdminInvite(
          request,
          env,
          parseAdminInviteRequest(await requestJson(request)),
        ),
        requestId,
        201,
      );
    }
    if (request.method === "GET" && path === "/v1/admin/registrations") {
      return json(await listContributorRegistrations(request, env), requestId);
    }
    if (request.method === "POST" && path === "/v1/admin/cleanup") {
      const result = await adminCleanup(request, env);
      await cleanupPendingRejectedDicomInputs(env, 50);
      return json(result, requestId);
    }
    const inviteId = idMatch(inviteRevokeRoute, path);
    if (request.method === "POST" && inviteId) {
      return json(await revokeInvite(request, env, inviteId), requestId);
    }
    const deviceId = idMatch(deviceRevokeRoute, path);
    if (request.method === "POST" && deviceId) {
      return json(await revokeDevice(request, env, deviceId), requestId);
    }
    const withdrawnUploadId = idMatch(uploadWithdrawRoute, path);
    if (request.method === "POST" && withdrawnUploadId) {
      return json(
        await withdrawUpload(request, env, withdrawnUploadId),
        requestId,
      );
    }

    throw new AppError("NOT_FOUND", 404, "Route was not found");
  } catch (error) {
    const appError =
      error instanceof AppError
        ? error
        : new AppError("INTERNAL", 500, "Unexpected control-plane error");
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

const worker = {
  fetch: fetchHandler,
  async scheduled(
    _controller: ScheduledController,
    env: Env,
    ctx: ExecutionContext,
  ): Promise<void> {
    ctx.waitUntil(
      Promise.allSettled([
        cleanupAbandoned(env),
        cleanupPendingRejectedDicomInputs(env, 50),
      ]).then((results) => {
        if (results.some((result) => result.status === "rejected")) {
          console.error(
            JSON.stringify({ event: "cleanup_failed", source: "scheduled" }),
          );
        }
      }),
    );
  },
} satisfies ExportedHandler<Env>;

export default worker;
