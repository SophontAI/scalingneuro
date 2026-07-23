import { env } from "cloudflare:workers";
import { createExecutionContext } from "cloudflare:test";
import { describe, expect, it } from "vitest";
import pages from "../src/pages";

const assets: Fetcher = {
  fetch: async () => new Response("asset"),
  connect: () => {
    throw new Error("Socket connections are not used by the static test asset binding");
  },
};

const pagesEnv = {
  ...env,
  ASSETS: assets,
};

describe("Pages advanced-mode wrapper", () => {
  it("redirects only the exact legacy production hostname", async () => {
    const response = await pages.fetch(
      new Request("https://scalingneuro.pages.dev/docs?source=legacy"),
      pagesEnv,
      createExecutionContext(),
    );
    expect(response.status).toBe(301);
    expect(response.headers.get("location")).toBe(
      "https://scalingneuro.com/docs?source=legacy",
    );
  });

  it("does not redirect Pages preview subdomains", async () => {
    const response = await pages.fetch(
      new Request("https://preview-hash.scalingneuro.pages.dev/docs"),
      pagesEnv,
      createExecutionContext(),
    );
    expect(response.status).toBe(200);
    expect(await response.text()).toBe("asset");
  });

  it("never exposes production API bindings on preview hostnames", async () => {
    const response = await pages.fetch(
      new Request("https://preview-hash.scalingneuro.pages.dev/health"),
      pagesEnv,
      createExecutionContext(),
    );
    expect(response.status).toBe(404);
    expect(response.headers.get("cache-control")).toBe("no-store");
  });

  it("does not expose the API on the retired ingestion hostname", async () => {
    const response = await pages.fetch(
      new Request("https://ingest.scalingneuro.com/health"),
      pagesEnv,
      createExecutionContext(),
    );
    expect(response.status).toBe(404);
    expect(response.headers.get("cache-control")).toBe("no-store");
  });

  it("serves the API on loopback for complete local verification", async () => {
    const response = await pages.fetch(
      new Request("http://127.0.0.1:8788/health"),
      pagesEnv,
      createExecutionContext(),
    );
    expect(response.status).toBe(200);
    expect(await response.json()).toMatchObject({
      status: "ok",
      service: "scaling-neuro-sync",
    });
  });
});
