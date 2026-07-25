# neuro-sync release contract

The public installation paths are:

- `https://scalingneuro.org/install.sh`
- `https://scalingneuro.org/install.ps1`

Each release builds one universal package for Apple-silicon and Intel macOS,
plus Windows x86-64 and static Linux x86-64 packages. A package contains only
the executable and the two project licenses. It does not bundle Python, a
DICOM converter, a GUI, a browser component, a cloud CLI, or cloud credentials.

Installers embed the exact package name and SHA-256, download over HTTPS, reject
any mismatch before installation, activate the verified version under the user
account, update only the user PATH, and return to the shell. They never run
`neuro-sync` automatically.

The manual release workflow:

1. verifies the requested version matches `client/Cargo.toml`;
2. runs Rust formatting, clippy, and tests;
3. runs Worker type-check and tests;
4. builds the three locked Rust targets;
5. packages the executable and licenses;
6. renders checksum-pinned installers and `latest.json`;
7. writes `SHA256SUMS`; and
8. publishes `client-vX.Y.Z` from the current commit.

The production deployment requires the latest release tag to match the client
version in source, then verifies `SHA256SUMS` before restoring that release into
the Pages build.

A semantic client version bump is required for behavior or wire-contract
changes. Archive layout, identifier derivation, classifier selection,
deidentification, and request semantics also require their explicit contract or
policy version to change.
