# CBGPU Materials

[![CI](https://github.com/CBGPU-Engineering/CBGPU-materials/actions/workflows/ci.yml/badge.svg)](https://github.com/CBGPU-Engineering/CBGPU-materials/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/Rust-Axum-000000?logo=rust)](https://www.rust-lang.org/)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache%202.0-D22128)](LICENSE)

The authenticated Rust file-transfer service for CBGPU shared materials. It stores
file bytes on the server and uses the existing PostgreSQL metadata API for
authentication, ownership and row access policies. The Rust service replaced the
Python file server on November 16, 2026, using the same loopback port and storage.

- **Resumable uploads** — 8 MiB chunks with persistent offsets and quota reservations.
- **Recovery** — interrupted chunks roll back; a lost completion acknowledgement can be retried without losing the completed file.
- **Private downloads** — caller authentication and metadata access checks before streaming bytes, including HTTP range requests.
- **Storage accounting** — concurrent reservations share one quota; stale upload cleanup preserves active uploads and completed files.

## Getting started

Requires Rust 1.88 or later. Create an isolated storage directory and load your
private configuration using the variables in [.env.example](.env.example).

```bash
mkdir -p /tmp/cbgpu-materials-dev
cargo run --locked
```

The default listener is 127.0.0.1:8423. Writes default to disabled. For a new
isolated development instance, set CBGPU_MATERIALS_WRITES_ENABLED=1 and
CBGPU_MATERIALS_LEGACY_QUIESCED=1 after confirming no other writer uses that
directory. Never run multiple writers against the same files. Public client keys
and caller tokens preserve the existing metadata authorization boundary; the
service does not need a service-role key.

## Container

```bash
docker build -t cbgpu-materials-rust .
```

The image runs as UID 65532 by default and listens on container port 8421. Use
a private environment file, mount the storage directory at /data, and publish
only on loopback. For existing storage, run with its established non-root owner
and verify file permissions. The readiness probe checks writable storage and
metadata connectivity. The production website reaches this service through
/api/files; the file server is not an unauthenticated public storage endpoint.

## Verification

```bash
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
npm ci --ignore-scripts
npm run check
CBGPU_MATERIALS_TEST_IMAGE=cbgpu-materials-rust node scripts/test-rust-container.mts
```

Nine integration tests use temporary storage and a loopback metadata fixture,
covering recovery, caller isolation, cancellation, quotas, cleanup and range
downloads. The container test checks non-root execution with a read-only root
filesystem, readiness, authentication rejection and disabled writes. It does not
connect to production files or accounts.

## Project structure

```text
src/             Axum API, authentication transport and file storage
tests/           Isolated Rust transfer and storage contracts
scripts/         TypeScript publication, checksum and container checks
Dockerfile       Rust build and minimal Debian runtime
Cargo.lock       Reproducible Rust dependencies
```

The metadata schema is maintained in [CBGPU-backend](https://github.com/CBGPU-Engineering/CBGPU-backend).
[CBGPU-platform](https://github.com/CBGPU-Engineering/CBGPU-platform) assembles this component at MaterialShare/.
See [CONTRIBUTING.md](CONTRIBUTING.md) and [SECURITY.md](SECURITY.md).

## License

Licensed under [Apache 2.0](LICENSE). Third-party dependencies retain their own licenses.
