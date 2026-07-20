# neuro-sync release contract

The collaborator-facing path is the terminal installer at `https://scalingneuro.com/install.sh` or `https://scalingneuro.com/install.ps1`. Versioned archives are implementation assets consumed by those installers, not a manual-download onboarding path.

## Packages

Each release builds:

```text
neuro-sync-vX.Y.Z-macos-universal.zip
neuro-sync-vX.Y.Z-windows-x86_64.zip
neuro-sync-vX.Y.Z-linux-x86_64-musl-static.tar.gz
```

If Apple or Windows signing credentials are unavailable, the filename includes `-UNSIGNED-PILOT`; a signed but not notarized macOS build includes `-CODESIGNED-PILOT`.

Each package contains only:

- `neuro-sync` or `neuro-sync.exe`;
- `CONTRIBUTING-SCANS.md`;
- `LICENSE-MIT` and `LICENSE-APACHE`;
- `RELEASE.json` with client version, source commit, platform/runtime, signing state, and workstation-processing mode; and
- SPDX and CycloneDX SBOMs.

No local converter, Python runtime, GUI framework, browser component, cloud CLI, or reusable cloud credential is bundled. Pinned `dcm2niix` belongs to the independently versioned cluster-processor container.

## Installer behavior

The release workflow renders `install.sh` and `install.ps1` with the exact platform archive names and SHA-256 values embedded. Installers:

1. select the platform package;
2. download over HTTPS;
3. reject any SHA-256 mismatch before changing the installation;
4. extract to a staging directory under the user account;
5. atomically activate the verified version and update only the user PATH; and
6. return to the shell after printing `neuro-sync /path/to/dicom-export`.

They do not require administrator access, execute `neuro-sync`, start a browser, send telemetry, or collect registration details. Reinstalling the same version is safe. A verified previous version is restored if activation fails.

On macOS/Linux the default executable shim is `~/.local/bin/neuro-sync` and versions live under `~/.local/share/neuro-sync/versions/`. Windows installs under `%LOCALAPPDATA%\ScalingNeuro\neuro-sync\bin` and updates only the user PATH.

## Platform guarantees

Linux has one public x86-64 package. It is built for `x86_64-unknown-linux-musl` with the C runtime statically linked, so selecting a glibc-versus-musl download is never part of researcher onboarding. Ordinary CI and the release workflow both inspect the ELF and refuse a package with a runtime interpreter (`INTERP`) or dynamic dependency (`NEEDED`), then execute the binary and verify its exact version. It therefore has no distribution-provided glibc, musl, X11, Wayland, GTK, or desktop-portal dependency. The host must still be an x86-64 Linux system whose kernel and institutional controls permit user executables.

macOS combines Rust arm64 and x86-64 builds with `lipo`. If signing credentials are configured, the executable is hardened-runtime signed; if notary credentials are also configured, the archive is submitted to Apple and publication requires an accepted result.

Windows builds x86-64. If Authenticode credentials are configured, every executable/DLL in the package is signed and verified before packaging.

Terminal-first delivery minimizes installation surface but does not override institutional endpoint-security policy. Unsigned pilot names are explicit rather than implying trust they do not have.

## Publication

`latest.json` maps `macos`, `windows`, and `linux` to the exact versioned Pages URL, package SHA-256, signing state, and SBOM URLs. It also records the release-bound installer hashes. `SHA256SUMS` covers all packages, installers, SBOMs, and the index. Publication refuses unsafe paths, a mismatched version, an asset not represented in the checksums, or a Pages file over 25 MiB.

The GitHub `client-vX.Y.Z` release is the source used to restore public downloads during ordinary production site deploys. Publication requires the tag commit to equal current `main`. The canonical researcher URLs remain `scalingneuro.com`.

Release publication uses a two-phase production cutover. Phase one builds the candidate backend and site, captures the currently public static site plus `latest.json`, `SHA256SUMS`, installers, packages, and SBOMs from the canonical domain, verifies their complete inventories, and deploys those exact old public bytes with the candidate backend. The workflow extracts that exact preserved Linux client and requires it to fetch the contribution contract and complete a fresh terminal registration through phase one, preventing an old-installer/new-API compatibility gap. The freshly built candidate client and mixed non-PHI functional/structural Siemens fixture must then complete receipt, cluster processing, and same-folder replay against production. Only phase two publishes the candidate site, release index, installers, and platform bytes. D1 migrations and all-MR state are forward-only: after phase one, recovery redeploys the candidate backend as a forward-compatible bridge with the preserved prior site and downloads; it never restores a pre-v2 Worker over migrated state. A just-published GitHub release is returned to draft state if the public cutover does not converge.

## Release gates

Before publication:

1. Schema Python and strict-Ajv validation pass.
2. Rust formatting, clippy with warnings denied, and the complete test suite pass.
3. Worker type-check and all local Cloudflare lifecycle tests pass.
4. Processor archive, API, conversion, deterministic-output, recovery, and fake-server tests pass.
5. Unix/macOS and Windows installer tests prove user-level install, no automatic launch, repeat install, exact `--version`, and tamper rejection; native Windows CI also proves private state receives a protected current-account-only ACL.
6. Platform packages build from the locked Rust dependencies and satisfy runtime/signing checks; ordinary CI builds and executes the fully static Linux target before any release tag exists.
7. SBOMs, `latest.json`, installers, and `SHA256SUMS` are generated from the assembled packages.
8. The exact checksum-preserved public Linux client fetches the phase-one contribution contract and completes a new terminal registration before its installer can remain exposed with the new backend.
9. A synthetic non-PHI end-to-end smoke runs the newly built candidate binary against the phase-one backend, obtains a DICOM receipt, reaches processed state through the deployed queue, and proves same-folder replay before any candidate download becomes public.
10. The phase-two release cutover converges to the exact new site, index, checksums, installers, packages, and SBOMs; otherwise Pages restores the forward-compatible candidate backend with the captured prior static site and downloads.

Clean-machine tests should additionally cover paths with spaces/non-ASCII characters, network interruption and same-folder continuation, read-only/network-mounted exports, shell PATH refresh, and the operating system’s actual trust prompts.

## Versioning

A client semantic version bump is required for behavior or contract changes. Changes to archive layout, series identity, DICOM de-identification behavior, classifier threshold/evidence, or Worker request semantics also require the corresponding versioned schema/policy and compatibility handling. A de-identification policy change invalidates incompatible prepared archives; it must never silently continue older bytes.

The processor image pins its own application and `dcm2niix` versions. Updating it requires its source-archive digest, container build, vendor fixture baselines, derived metadata expectations, and output determinism tests to change in one reviewed release.
