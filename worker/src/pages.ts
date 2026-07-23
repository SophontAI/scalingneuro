import type { Env } from "./env";
import { fetchHandler } from "./index";

interface PagesEnv extends Env {
  ASSETS: Fetcher;
}

export default {
  async fetch(
    request: Request,
    env: PagesEnv,
    ctx: ExecutionContext,
  ): Promise<Response> {
    const url = new URL(request.url);
    if (url.hostname === "scalingneuro.pages.dev") {
      url.hostname = "scalingneuro.com";
      url.protocol = "https:";
      url.port = "";
      return Response.redirect(url.toString(), 301);
    }
    const pathname = url.pathname;
    const isApiHost =
      url.hostname === "scalingneuro.com" ||
      url.hostname === "localhost" ||
      url.hostname === "127.0.0.1" ||
      url.hostname === "[::1]";
    const isApiPath = pathname === "/health" || pathname.startsWith("/v1/");
    if (isApiHost && isApiPath) {
      return fetchHandler(request, env, ctx);
    }
    if (isApiPath) {
      return new Response("Not found\n", {
        status: 404,
        headers: {
          "cache-control": "no-store",
          "content-type": "text/plain; charset=utf-8",
          "x-content-type-options": "nosniff",
        },
      });
    }
    return env.ASSETS.fetch(request);
  },
} satisfies ExportedHandler<PagesEnv>;
