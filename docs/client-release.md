# Client release process

`.github/workflows/release-client.yml` creates traceable, checksum-verified pilot packages. A tag build, or a manual run with **publish release** enabled, also rebuilds the source-matched Pages Worker and static bundle and publishes the packages under `https://scalingneuro.com/downloads/`. Publication is allowed only from the current `main` commit and applies pending D1 migrations before replacing the Pages deployment; it does not mutate R2 archive objects.

## Release inputs and outputs

Trigger the workflow with a `client-vX.Y.Z` tag or manually with a version matching `client/Cargo.toml`. A manual unpublished run may build from another branch for smoke testing, but any GitHub/Pages publication must point at the current `main` commit. The release packages are:

```text
neuro-sync-vX.Y.Z-macos-universal.zip
neuro-sync-vX.Y.Z-windows-x86_64.zip
neuro-sync-vX.Y.Z-linux-x86_64.tar.gz
```

If Apple or Windows signing credentials are unavailable, that platform’s filename is changed to include `-UNSIGNED-PILOT` before the extension. Unsigned packages must never be presented as a general public release.

A signed but non-notarized macOS archive is named `-CODESIGNED-PILOT.zip`. Only a macOS archive that passes both hardened-runtime signing and Apple notarization receives the suffix-free filename. This keeps “signed” from being mistaken for the lower-friction Gatekeeper experience collaborators expect.

Each package contains the `neuro-sync` executable, the platform converter at `libexec/dcm2niix`, the converter's redistribution notice, the project `LICENSE-MIT` and `LICENSE-APACHE` texts, this onboarding guide, a `RELEASE.json` recording the source commit and converter archive digest, and SPDX and CycloneDX SBOMs. The release also publishes a portable `latest.json` index and a top-level `SHA256SUMS` over the final packages, SBOMs, and index.

The public release step writes `/downloads/latest.json`, mapping `macos`, `windows`, and `linux` to the exact versioned URL, SHA-256, signing state, and SBOM URLs. The same index is attached to the GitHub release so an ordinary production deploy can restore the current downloads without trusting an older live Pages deployment. Publication refuses any individual asset larger than Cloudflare Pages' 25 MiB file limit. GitHub release attachment is secondary because this repository may be private; the Pages URLs are the collaborator-facing contract.

## Pinned converter

The workflow downloads official `rordenlab/dcm2niix` release assets at tag `v1.0.20260416` and rejects any digest mismatch before packaging:

| Asset | SHA-256 |
|---|---|
| `dcm2niix_lnx.zip` | `e88b40f6ebbcf9f47ebfdd7bb5f0127297cb7e8b06266a91a4642b5814031bd0` |
| `dcm2niix_mac.zip` | `51e909fca34db8198d8a917cb85a00e135841d32a3c51f1154ec8d5b874de852` |
| `dcm2niix_win.zip` | `969bca4fc41d5f82658acef9d0ed9cbfbd4114ec8e8668906910241fcbb2c048` |

The macOS converter is already a universal x86_64/arm64 Mach-O. The workflow builds both Rust targets and combines them with `lipo` into a universal `neuro-sync` executable. Windows includes the runtime DLLs shipped in the official converter archive.

The Linux client is built for `x86_64-unknown-linux-gnu`. Both the native XDG folder picker and the official converter use the desktop Linux runtime; the converter requires glibc 2.19 or newer, which is the bundle's actual compatibility floor. The release gate installs the Wayland development headers needed to compile the portal picker, while collaborator machines need a normal desktop portal/Wayland runtime rather than a compiler toolchain.

Updating dcm2niix requires a separate reviewed change to the version, all three official asset digests, converter regression fixtures, metadata policy if output semantics changed, and the `converter_version` constant in the scan-sidecar schema.

## Optional signing

Apple signing is enabled only when all of these repository secrets are present:

- `APPLE_CERTIFICATE_P12_BASE64`
- `APPLE_CERTIFICATE_PASSWORD`
- `APPLE_SIGNING_IDENTITY`

The workflow imports the temporary keychain, signs both bundled Mach-O executables with hardened runtime and timestamping, verifies them, then deletes the keychain. A code-signed-only archive remains explicitly labeled `CODESIGNED-PILOT`.

Apple ZIP notarization is enabled only when signing succeeded and all of these additional repository secrets are present:

- `APPLE_NOTARY_KEY_P8_BASE64`
- `APPLE_NOTARY_KEY_ID`
- `APPLE_NOTARY_ISSUER_ID`

The workflow creates the final ZIP, submits that exact archive with `notarytool --wait`, and publishes it only after Apple accepts it. ZIP files cannot carry a stapled ticket, so the notarized executables rely on Gatekeeper’s online ticket lookup; a future `.pkg`, `.dmg`, or `.app` release can add stapling. The temporary API key file is deleted even if submission fails.

Windows Authenticode signing is enabled only when all of these repository secrets are present:

- `WINDOWS_CERTIFICATE_PFX_BASE64`
- `WINDOWS_CERTIFICATE_PASSWORD`

The workflow signs the Rust executable and bundled executable/DLL runtime files with SHA-256 and an RFC 3161 timestamp, then verifies the signatures. Secrets are written only to runner-temporary files and removed before artifact upload.

## Release gate

Before sharing a package:

1. The release workflow's blocking verification job must pass schema/policy consistency, strict Ajv compilation, Rust formatting/clippy/tests, and Worker type/tests.
2. The release workflow must verify the pinned converter and produce SBOMs and `SHA256SUMS`.
3. Run clean-machine smoke tests for each promised OS: enrollment, native folder selection, dry run, accepted/held separation, upload, forced interruption, resume, commit, and report.
4. Inspect the stored sidecar and manifest for schema validity, metadata retention, absence of seeded PHI, and exact local/R2 hashes.
5. Prefer notarized macOS and Authenticode-signed Windows builds for collaborators. Keep `UNSIGNED-PILOT` and `CODESIGNED-PILOT` builds invite-only and explicitly labeled.

The workflow can create a GitHub prerelease when requested. Manual builds do not publish unless **publish release** is explicitly enabled; `client-vX.Y.Z` tags publish the corresponding versioned pilot downloads automatically.
