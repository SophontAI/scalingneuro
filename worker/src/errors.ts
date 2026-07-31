export type ErrorCode =
  | "INVALID_REQUEST"
  | "UNAUTHORIZED"
  | "DEVICE_REVOKED"
  | "CLIENT_UPDATE_REQUIRED"
  | "CONSENT_POLICY_UPDATE_REQUIRED"
  | "ARCHIVE_ACCESS_POLICY_UPDATE_REQUIRED"
  | "NOT_FOUND"
  | "UPLOAD_NOT_WRITABLE"
  | "DUPLICATE_BUNDLE"
  | "OBJECT_MISSING"
  | "OBJECT_MISMATCH"
  | "CREDENTIALS_UNAVAILABLE"
  | "STORAGE_UNAVAILABLE"
  | "QUOTA_EXCEEDED"
  | "CONFLICT"
  | "INTERNAL";

export class AppError extends Error {
  readonly code: ErrorCode;
  readonly status: number;
  readonly details: Readonly<Record<string, unknown>> | undefined;

  constructor(
    code: ErrorCode,
    status: number,
    message: string,
    details?: Readonly<Record<string, unknown>>,
  ) {
    super(message);
    this.name = "AppError";
    this.code = code;
    this.status = status;
    this.details = details;
  }
}

export function invalid(message: string): never {
  throw new AppError("INVALID_REQUEST", 400, message);
}
