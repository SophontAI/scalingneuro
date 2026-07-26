import type { SubmittedArchiveAccessRequest } from "./archive-access";
import type { Env } from "./env";

const NOTIFICATION_URL =
  "https://archive-access-notifier.internal/v1/archive-access-notification";

export async function notifyArchiveAccessRequest(
  env: Env,
  notification: SubmittedArchiveAccessRequest["notification"],
): Promise<void> {
  const response = await env.ARCHIVE_ACCESS_NOTIFIER.fetch(NOTIFICATION_URL, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(notification),
  });
  if (!response.ok) {
    throw new Error(`Archive access notifier returned ${response.status}`);
  }
}

export function logArchiveAccessNotificationFailure(
  requestId: string,
  error: unknown,
): void {
  console.error(
    JSON.stringify({
      event: "archive_access_notification_failed",
      request_id: requestId,
      error: error instanceof Error ? error.name : "UnknownError",
    }),
  );
}
