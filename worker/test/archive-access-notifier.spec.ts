import { describe, expect, it, vi } from "vitest";
import {
  archiveAccessNotifierFetch,
  buildArchiveAccessNotificationEmail,
  parseArchiveAccessNotification,
} from "../src/archive-access-notifier";

function notification(): Record<string, unknown> {
  return {
    request_id: "718ac186-a7f9-4cf4-b16c-8768f80338c4",
    contact_name: "Example <Researcher>",
    contact_email: "researcher@example.edu",
    institution_name: "Example & University",
    lab_name: "Example Neuroimaging Lab",
    plans_to_contribute: true,
    contributor_attestation: true,
    accepted_contribution_policy_version: "open-epi-3.0.0",
    accepted_data_use_policy_version: "archive-access-1.0.0",
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
    expect(email.text).toContain("archive-access-1.0.0");
    expect(email.text).toContain("Plans to contribute data: Yes");
    expect(email.text).toContain("Accepted contribution policy: open-epi-3.0.0");
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

  it("clearly identifies a requester who does not plan to contribute", () => {
    const parsed = parseArchiveAccessNotification({
      ...notification(),
      plans_to_contribute: false,
      contributor_attestation: false,
      accepted_contribution_policy_version: null,
    });
    const email = buildArchiveAccessNotificationEmail(parsed);

    expect(email.text).toContain("Plans to contribute data: No");
    expect(email.text).toContain("Contributor attestation: Not applicable");
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
