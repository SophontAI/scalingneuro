export interface Env extends ConfiguredEnv {
  ARCHIVE_ACCESS_ADMIN_TOKEN: string;
  R2_PARENT_SECRET_ACCESS_KEY: string;
  SITE_KEY_ENCRYPTION_KEY_B64: string;
}

export interface DeviceContext {
  id: string;
  site_id: string;
  project_id: string;
  accepted_consent_policy_version: string;
  current_consent_policy_version: string;
  project_name: string;
  upload_quota_bytes: number | null;
  self_service: boolean;
}

export type UploadStatus =
  "created" | "uploading" | "committed" | "expired" | "withdrawn";

export type IngestFormat = "dicom-series-v1";
