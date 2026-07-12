export type ErrorCode =
  | "INVALID_REQUEST"
  | "UNAUTHORIZED"
  | "INVALID_INVITE"
  | "DEVICE_REVOKED"
  | "CONSENT_POLICY_UPDATE_REQUIRED"
  | "NOT_FOUND"
  | "UPLOAD_NOT_WRITABLE"
  | "DUPLICATE_BUNDLE"
  | "OBJECT_MISSING"
  | "OBJECT_MISMATCH"
  | "CREDENTIALS_UNAVAILABLE"
  | "STORAGE_UNAVAILABLE"
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
