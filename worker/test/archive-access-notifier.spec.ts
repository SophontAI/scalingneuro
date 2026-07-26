import { describe, expect, it, vi } from "vitest";
import {
  archiveAccessNotifierFetch,
  buildArchiveAccessNotificationEmail,
  parseArchiveAccessNotification,
} from "../src/archive-access-notifier";

function notification(): Record<string, string> {
  return {
    request_id: "718ac186-a7f9-4cf4-b16c-8768f80338c4",
    contact_name: "Example <Researcher>",
    contact_email: "researcher@example.edu",
    institution_name: "Example & University",
    lab_name: "Example Neuroimaging Lab",
    submitted_at: "2026-07-26T17:00:00.000Z",
  };
}

describe("archive access notifier", () => {
  it("builds a safe notification restricted to the admin mailbox", () => {
    const parsed = parseArchiveAccessNotification(notification());
    const email = buildArchiveAccessNotificationEmail(parsed);

    expect(email.to).toEqual(["scottibrain@gmail.com"]);
    expect(email.from).toEqual({
      email: "archive-access@scalingneuro.org",
      name: "Scaling Neuro",
    });
    expect(email.subject).toBe(
      "New archive access request: Example & University",
    );
    expect(email.text).toContain(parsed.request_id);
    expect(email.html).toContain("Example &lt;Researcher&gt;");
    expect(email.html).toContain("Example &amp; University");
    expect(email.html).not.toContain("Example <Researcher>");
  });

  it("sends a valid internal notification", async () => {
    const send = vi.fn(async () => ({ messageId: "test-message-id" }));
    const response = await archiveAccessNotifierFetch(
      new Request(
        "https://archive-access-notifier.internal/v1/archive-access-notification",
        {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify(notification()),
        },
      ),
      { ADMIN_EMAIL: { send } },
    );

    expect(response.status).toBe(204);
    expect(send).toHaveBeenCalledOnce();
  });

  it("rejects malformed notification requests without sending email", async () => {
    const send = vi.fn(async () => ({ messageId: "unexpected" }));
    const response = await archiveAccessNotifierFetch(
      new Request(
        "https://archive-access-notifier.internal/v1/archive-access-notification",
        {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ ...notification(), unexpected: "field" }),
        },
      ),
      { ADMIN_EMAIL: { send } },
    );

    expect(response.status).toBe(400);
    expect(send).not.toHaveBeenCalled();
  });

  it("reports a provider failure to the caller", async () => {
    const send = vi.fn(async () => {
      throw new Error("provider unavailable");
    });
    const response = await archiveAccessNotifierFetch(
      new Request(
        "https://archive-access-notifier.internal/v1/archive-access-notification",
        {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify(notification()),
        },
      ),
      { ADMIN_EMAIL: { send } },
    );

    expect(response.status).toBe(502);
    expect(send).toHaveBeenCalledOnce();
  });
});
