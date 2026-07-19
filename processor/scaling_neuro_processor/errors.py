from __future__ import annotations


class ProcessorError(Exception):
    """A code-only error safe to report to the control plane."""

    def __init__(self, code: str, *, retryable: bool = False):
        super().__init__(code)
        self.code = code
        self.retryable = retryable


class LeaseLost(ProcessorError):
    def __init__(self) -> None:
        super().__init__("LEASE_LOST", retryable=True)


class ApiFailure(ProcessorError):
    def __init__(self, code: str = "CONTROL_PLANE_UNAVAILABLE") -> None:
        super().__init__(code, retryable=True)


class InvalidJob(ProcessorError):
    def __init__(self) -> None:
        super().__init__("INVALID_JOB", retryable=False)


class InvalidArchive(ProcessorError):
    def __init__(self, code: str = "INVALID_DICOM_ARCHIVE") -> None:
        super().__init__(code, retryable=False)


class InvalidNifti(ProcessorError):
    def __init__(self, code: str = "INVALID_FUNCTIONAL_NIFTI") -> None:
        super().__init__(code, retryable=False)


class ConverterFailure(ProcessorError):
    def __init__(
        self, code: str = "DCM2NIIX_FAILED", *, retryable: bool = False
    ) -> None:
        super().__init__(code, retryable=retryable)


class CapacityFailure(ProcessorError):
    """The claimed job is valid but this processor lacks safe local capacity."""

    def __init__(self, code: str = "LOW_DISK_SPACE") -> None:
        super().__init__(code, retryable=True)
