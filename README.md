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

### Native migration CLI

`cbgpu-materials-migrate` implements the inventory procedure above without
changing PostgreSQL metadata. It uses the same private S3 environment settings.
The normal server remains Cargo's default binary. The CLI prefix precedence is
`--prefix`, then `CBGPU_MATERIALS_S3_PREFIX`, then `materials-private-v1`; it
never copies into an empty/public root. Reports include the local root and a
SHA-256 fingerprint of the JSON array `[bucket, region, prefix, endpoint-or-null]`
(without credentials), alongside the exact inventory hash. Build the separate tool with
`cargo build --locked --bin cbgpu-materials-migrate`.

First review a read-only-source plan. Reports must be **new files outside the
storage directory**, and the manifest must use the exact canonical format above:

```bash
cargo run --locked --bin cbgpu-materials-migrate -- \
  --direction local-to-s3 --manifest /private/inventory.tsv \
  --local-root /private/original-materials --report /private/plan.json
```

The plan verifies source bytes and any existing destination objects. It copies
one local source at a time to a private temporary snapshot for verification;
therefore allow free temporary disk space up to the largest file. All transfers
stream with bounded buffers. The tool rejects symlinked local source components
on Unix, invalid/duplicate/unsorted UUID entries, noncanonical sizes/hashes and
unexpected paths. It never follows a filename supplied by a browser.

After independently stopping **all** writers (including remote writers), draining
uploads, preserving the source backup and reviewing the printed manifest hash:

```bash
cargo run --locked --bin cbgpu-materials-migrate -- \
  --direction local-to-s3 --manifest /private/inventory.tsv \
  --local-root /private/original-materials --report /private/copy.json \
  --apply --confirm-writers-fenced \
  --expected-manifest-sha256 REPLACE_WITH_REVIEWED_64_HEX_HASH
```

Existing same-UUID objects are read back and must match both size and SHA-256;
they are skipped only after verification. Differing objects cause failure, never
overwrite. Each newly copied object is also fully read back and verified. No
original object or file is removed. A failure can leave already verified copies;
rerun with the same pinned inventory and a **new** report filename. Reports are
atomically created without replacing existing files and are mode 0600 on Unix.
They explicitly record `external_writers_inspected: false`: the fence flag is an
operator acknowledgement, **not** remote writer detection or a distributed lock.
Do not use the tool while any writer can mutate the source or destination.

For rollback after S3-only writes, independently create a **current** authorized
inventory, then copy into a **nonexistent** local destination:

```bash
cargo run --locked --bin cbgpu-materials-migrate -- \
  --direction s3-to-local --manifest /private/current-inventory.tsv \
  --local-root /private/new-restored-materials --report /private/reverse.json \
  --apply --confirm-writers-fenced \
  --expected-manifest-sha256 REPLACE_WITH_REVIEWED_64_HEX_HASH
```

Reverse copy creates a private destination receipt bound to the exact inventory
hash and absolute root. It refuses an existing root by default; only an explicit
`--resume` accepts that tool-created matching receipt. Completed files are
published without replacing an existing path and rehashed from local disk.
Temporary files are removed on reported copy errors; a killed process can leave
private temporary files for later operator review. Original storage is never reused as the
reverse destination. Changing the server configuration, restarting workers,
restoring metadata or opening admissions remains a separate reviewed operation.
On non-Unix systems, filesystem ACL and reparse-point hardening require separate
validation before migration; the operating-source handoff was tested on Linux.

```bash
cargo test --locked --test migration
CBGPU_MATERIALS_TEST_S3_ENDPOINT=http://127.0.0.1:PORT \
  cargo test --locked --test migration minio_full -- --ignored
```

The isolated MinIO test executes the actual CLI across a full synthetic inventory
(including a 9 MiB multipart object), verifies forward/reverse/resume reports,
checks exact byte hashes, and proves corrupted sources, differing destinations
and existing reverse roots are refused. No operating inventory, AWS bucket,
credentials, metadata migration or RDS connection is used by these tests.

### Scoped school and temporary-guest uploads (source candidate)

Server `CBGPU_MULTISCHOOL_V1=1|true` or `CBGPU_ACCOUNT_V2=1|true`
enables fresh caller-session and school-resource checks. Only canonical
`0|false|1|true` values are accepted; browser public flags never enable server
authority. Both server flags default off for the legacy API.

In scoped mode, `POST /materials` requires caller Bearer authentication and an
`X-CBGPU-School-Id` candidate. Before reading the body, the server verifies the
fresh session and `cbgpu_self_resource_access(target_school)` storage permission.
The bounded JSON contains `upload_id` (client-generated UUID for retry), `title`,
`file_name`, `content_type`, `size_bytes`, and `tags`. Identity and visibility
fields are rejected. The caller-JWT metadata insert fixes uploader, selected
school, pending status, `SCHOOL` visibility and an empty allowed-school list.
Identical same-owner retries reuse their existing metadata; conflicting UUIDs
fail closed. No service-role credentials are used.

Chunk/resume requests authorize the school stored in the owned metadata, not a
client override. Completion checks authority again: revoked or expired grants
cannot publish a ready file, while pending bytes and owned records remain for
explicit owner recovery/deletion. Owned completed downloads and deletion require
fresh identity, independently of expired school-resource permission.

This requires the reviewed account/school migrations (including scoped material
RLS) and a paired web/file-service handoff. Existing operating immutable releases
and `/api/files` origin are unchanged by source preparation. Tests use disposable
storage and synthetic metadata, never operating user files.
