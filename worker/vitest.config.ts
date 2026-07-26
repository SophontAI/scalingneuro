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
          ARCHIVE_ACCESS_ADMIN_TOKEN:
            "test-archive-access-admin-token-0000000000000000",
          R2_PARENT_SECRET_ACCESS_KEY: "test-parent-secret-access-key",
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
