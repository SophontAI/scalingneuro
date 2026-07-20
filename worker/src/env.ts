export interface Env {
  DB: D1Database;
  ARCHIVE: R2Bucket;
  R2_ACCOUNT_ID: string;
  R2_PARENT_ACCESS_KEY_ID: string;
  R2_BUCKET_NAME: string;
  R2_PARENT_SECRET_ACCESS_KEY: string;
  ADMIN_API_TOKEN: string;
  PROCESSOR_API_TOKEN: string;
  SITE_KEY_ENCRYPTION_KEY_B64: string;
  CREDENTIAL_TTL_SECONDS?: string;
  UPLOAD_TTL_SECONDS?: string;
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

export type IngestFormat = "nifti-v1" | "dicom-series-v1";
