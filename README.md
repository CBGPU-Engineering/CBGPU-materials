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

## Optional private S3 storage

`CBGPU_MATERIALS_STORAGE_BACKEND` defaults to `local`. An explicit `s3` selects
completed objects through the same Rust storage interface; resumable chunks,
reservations, the exclusive process lock and deletion retry tickets remain on a
persistent local staging volume. Configure the bucket, region and nonempty
prefix in [.env.example](.env.example). Provision the private bucket separately;
the service never creates a bucket, changes ACLs, makes objects public or returns
presigned URLs. Existing authenticated `/materials/:id/content` URLs and fresh
caller-bearer PostgreSQL RLS checks remain the only download path.

The implementation pins Apache's maintained
[`object_store` 0.14.2](https://docs.rs/object_store/0.14.2/object_store/)
Local/S3 abstraction and SigV4 transport, rather than implementing signing.
Use standard AWS environment credentials or an instance/task role with access
restricted to the configured bucket/prefix: ListBucket (prefix condition),
GetObject, PutObject, DeleteObject and multipart upload/abort permissions.
TLS certificate verification is enabled. Plain HTTP requires an explicit opt-in
and a private fixture endpoint. Credentials are never included in file responses.

S3 writes also require `CBGPU_MATERIALS_S3_SINGLE_WRITER=1`. This is an
acknowledgement, not a distributed lock: run **one writer** per bucket/prefix,
with its persistent staging volume, including during deployments. Horizontal
multi-writer quota enforcement is not implemented. S3 requires a consistent
list/head implementation for accounting. Quota includes completed objects and
local upload reservations; provider-side lifecycle must remove abandoned
multipart uploads after crashes. Metadata deletion creates a durable private
retry ticket before attempting object deletion; maintenance retries failed
physical deletions without exposing orphaned objects.

### Migration and rollback verification

Changing configuration does **not** move or import files. Before any cutover:

1. Hold new uploads/deletes, drain active uploads, and back up metadata plus the
   complete local staging/storage directory. Do not delete or rename originals.
2. Produce a private inventory of `(material UUID, byte length, SHA-256)` for every
   ready metadata row and its regular local file. Reconcile missing/orphaned files
   separately; never infer ownership from object names. Copy to the new private
   prefix with the original UUID key layout: `PREFIX/first-two-hex/uuid-without-dashes`.
3. Read **every entire object back** and compare length and SHA-256 exactly. The
   Rust `Objects::sha256` helper streams the actual bytes. ETags, multipart ETags
   and the uploaded `cbgpu-sha256` metadata are not independent byte proofs.
   Canonical inventory bytes are UUID-sorted UTF-8 lines: lowercase hyphenated
   UUID, TAB, decimal byte length, TAB, lowercase SHA-256, LF (no header).
   Hash those exact bytes using SHA-256 and retain them with the metadata
   backup and configuration; record both source and destination inventory hashes.
4. Verify zero pending uploads/deletion tickets, identical counts/hashes, private
   unsigned access rejection and owner/foreign/expired-session API checks before
   switching the one writer. Retain the original local directory read-only.
5. A rollback to the original local root is safe only before any S3-only writes.
   After new writes, hold admissions/drain again and copy the current authorized
   object inventory into a **new** local root, verify every SHA-256 plus the sorted
   inventory hash, and restore local mode against that new root. Never point at
   a stale original directory or overwrite the only surviving bytes.

No operating migration, AWS account, IAM policy, KMS setup, bucket lifecycle or
RDS connection has been tested by this source change. Existing PostgreSQL
metadata schema, IDs, ownership and school authorization are unchanged. The S3
fixture proves compatible object transfer, not AWS/RDS production readiness.

For an isolated MinIO fixture with synthetic credentials only, run:

```bash
CBGPU_MATERIALS_TEST_S3_ENDPOINT=http://127.0.0.1:PORT \
  cargo test --locked --test rust_transfer private_s3 -- --ignored
```

The ignored test requires a loopback endpoint, creates a unique private fixture
bucket, and never reads AWS credential environment variables. It exercises real
multipart upload/read-back SHA-256, HTTP ranges, unauthenticated object denial,
caller isolation, recovery after metadata acknowledgement failure and quotas.

The reviewed fixture used MinIO upstream commit
`7aac2a2c5b7c882e68c1ce017d8256be2feea27f`, built from its official source
inside a disposable Go 1.24 container, with a loopback-only listener and a
128 MiB tmpfs. It also checks conditional GET/HEAD, immediate RLS revocation,
and durable deletion recovery after a provider authorization failure. This
external test dependency is not application source or a production requirement.
