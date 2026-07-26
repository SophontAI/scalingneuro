const NOTIFICATION_PATH = "/v1/archive-access-notification";
const ADMIN_EMAIL = "scottibrain@gmail.com";
const SENDER_EMAIL = "archive-access@scalingneuro.org";
const UUID =
  /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/u;

interface NotificationEnv {
  ADMIN_EMAIL: SendEmail;
}

export interface ArchiveAccessNotification {
  request_id: string;
  contact_name: string;
  contact_email: string;
  institution_name: string;
  lab_name: string;
  submitted_at: string;
}

function requiredText(
  value: unknown,
  label: string,
  maximum: number,
): string {
  if (typeof value !== "string") {
    throw new TypeError(`${label} must be text`);
  }
  const normalized = value.trim().replace(/\s+/gu, " ");
  if (normalized.length < 2 || normalized.length > maximum) {
    throw new TypeError(`${label} has an invalid length`);
  }
  return normalized;
}

export function parseArchiveAccessNotification(
  value: unknown,
): ArchiveAccessNotification {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    throw new TypeError("Notification body must be an object");
  }
  const input = value as Record<string, unknown>;
  const expected = new Set([
    "request_id",
    "contact_name",
    "contact_email",
    "institution_name",
    "lab_name",
    "submitted_at",
  ]);
  if (
    Object.keys(input).length !== expected.size ||
    Object.keys(input).some((key) => !expected.has(key))
  ) {
    throw new TypeError("Notification body has invalid fields");
  }
  const requestId = requiredText(input.request_id, "Request ID", 36);
  if (!UUID.test(requestId)) {
    throw new TypeError("Request ID is invalid");
  }
  const submittedAt = requiredText(input.submitted_at, "Submitted time", 40);
  if (Number.isNaN(Date.parse(submittedAt))) {
    throw new TypeError("Submitted time is invalid");
  }
  return {
    request_id: requestId,
    contact_name: requiredText(input.contact_name, "Contact name", 120),
    contact_email: requiredText(input.contact_email, "Contact email", 254),
    institution_name: requiredText(
      input.institution_name,
      "Institution",
      160,
    ),
    lab_name: requiredText(input.lab_name, "Lab", 160),
    submitted_at: submittedAt,
  };
}

function escapeHtml(value: string): string {
  return value
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");
}

export function buildArchiveAccessNotificationEmail(
  notification: ArchiveAccessNotification,
): EmailMessageBuilder {
  const fields = [
    ["Name", notification.contact_name],
    ["Work email", notification.contact_email],
    ["Institution", notification.institution_name],
    ["Lab", notification.lab_name],
    ["Submitted", notification.submitted_at],
    ["Request ID", notification.request_id],
  ] as const;
  const textFields = fields.map(([label, value]) => `${label}: ${value}`);
  const htmlFields = fields
    .map(
      ([label, value]) =>
        `<tr><th align="left">${escapeHtml(label)}</th><td>${escapeHtml(value)}</td></tr>`,
    )
    .join("");
  const approveCommand =
    `./scripts/archive-access-admin.sh approve ${notification.request_id}`;
  const rejectCommand =
    `./scripts/archive-access-admin.sh reject ${notification.request_id}`;

  return {
    to: [ADMIN_EMAIL],
    from: { email: SENDER_EMAIL, name: "Scaling Neuro" },
    subject: `New archive access request: ${notification.institution_name}`,
    text: [
      "A new Scaling Neuro archive access request is pending review.",
      "",
      ...textFields,
      "",
      "Approve:",
      approveCommand,
      "",
      "Reject:",
      rejectCommand,
      "",
      "No archive credentials have been issued.",
    ].join("\n"),
    html: [
      "<h2>New Scaling Neuro archive access request</h2>",
      "<p>A new request is pending review.</p>",
      `<table cellpadding="6" cellspacing="0">${htmlFields}</table>`,
      "<p><strong>Approve</strong></p>",
      `<pre>${escapeHtml(approveCommand)}</pre>`,
      "<p><strong>Reject</strong></p>",
      `<pre>${escapeHtml(rejectCommand)}</pre>`,
      "<p>No archive credentials have been issued.</p>",
    ].join(""),
  };
}

async function readJson(request: Request): Promise<unknown> {
  const contentType = request.headers
    .get("content-type")
    ?.split(";", 1)[0]
    ?.trim()
    .toLowerCase();
  if (contentType !== "application/json") {
    throw new TypeError("Content-Type must be application/json");
  }
  const text = await request.text();
  if (new TextEncoder().encode(text).byteLength > 16 * 1024) {
    throw new TypeError("Notification body is too large");
  }
  return JSON.parse(text) as unknown;
}

export async function archiveAccessNotifierFetch(
  request: Request,
  env: NotificationEnv,
): Promise<Response> {
  const url = new URL(request.url);
  if (request.method !== "POST" || url.pathname !== NOTIFICATION_PATH) {
    return new Response("Not found\n", { status: 404 });
  }
  let notification: ArchiveAccessNotification;
  try {
    notification = parseArchiveAccessNotification(
      await readJson(request),
    );
  } catch (error) {
    console.warn(
      JSON.stringify({
        event: "archive_access_notification_rejected",
        error: error instanceof Error ? error.name : "UnknownError",
      }),
    );
    return new Response("Invalid notification\n", { status: 400 });
  }
  try {
    const result = await env.ADMIN_EMAIL.send(
      buildArchiveAccessNotificationEmail(notification),
    );
    console.info(
      JSON.stringify({
        event: "archive_access_notification_sent",
        request_id: notification.request_id,
        message_id: result.messageId,
      }),
    );
    return new Response(null, { status: 204 });
  } catch (error) {
    console.error(
      JSON.stringify({
        event: "archive_access_notification_send_failed",
        request_id: notification.request_id,
        error: error instanceof Error ? error.name : "UnknownError",
      }),
    );
    return new Response("Notification delivery failed\n", { status: 502 });
  }
}

export default {
  fetch: archiveAccessNotifierFetch,
} satisfies ExportedHandler<NotificationEnv>;
