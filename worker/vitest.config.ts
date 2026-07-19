import {
  readD1Migrations,
  cloudflareTest,
} from "@cloudflare/vitest-pool-workers";
import { fileURLToPath } from "node:url";
import { defineConfig } from "vitest/config";

export default defineConfig({
  plugins: [
    cloudflareTest(async () => ({
      wrangler: { configPath: "./wrangler.jsonc" },
      miniflare: {
        bindings: {
          TEST_MIGRATIONS: await readD1Migrations(
            fileURLToPath(new URL("./migrations", import.meta.url)),
          ),
          SERVICE_VERSION: "test",
          R2_ACCOUNT_ID: "test-account",
          R2_PARENT_ACCESS_KEY_ID: "test-parent-key",
          R2_BUCKET_NAME: "scaling-neuro-test",
          R2_PARENT_SECRET_ACCESS_KEY: "test-parent-secret-access-key",
          ADMIN_API_TOKEN: "test-admin-token-with-sufficient-entropy",
          PROCESSOR_API_TOKEN:
            "test-processor-token-with-sufficient-entropy",
          SITE_KEY_ENCRYPTION_KEY_B64:
            "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=",
          CREDENTIAL_TTL_SECONDS: "900",
          UPLOAD_TTL_SECONDS: "86400",
        },
      },
    })),
  ],
  test: {
    setupFiles: ["./test/apply-migrations.ts"],
    restoreMocks: true,
  },
});
