# CBGPU Materials

[![CI](https://github.com/CBGPU-Engineering/CBGPU-materials/actions/workflows/ci.yml/badge.svg)](https://github.com/CBGPU-Engineering/CBGPU-materials/actions/workflows/ci.yml)
[![FastAPI](https://img.shields.io/badge/FastAPI-File%20API-009688?logo=fastapi)](https://fastapi.tiangolo.com)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache%202.0-D22128)](LICENSE)

The authenticated file-byte API for CBGPU shared materials. Searchable metadata
and ownership live in Supabase; file bytes are stored locally under UUID paths.
The service supports resumable uploads and quota accounting.

## Local environment

```bash
git clone https://github.com/CBGPU-Engineering/CBGPU-materials.git
cd CBGPU-materials
python3 -m venv .venv
.venv/bin/pip install -r requirements.txt
cp .env.example .env
```

Set your own isolated backend URL/public client key, origins and storage path.
Load the environment before starting `uvicorn app:app --host 127.0.0.1 --port 8421`.
For containers, use `docker build -t cbgpu-materials .` and a private `--env-file`.
Never point a development fixture at production student files.

The production web application reaches this API through `/api/files`; internal
loopback port 8421 is not an independently exposed public storage endpoint.
The backend schema is maintained in [CBGPU-backend](https://github.com/CBGPU-Engineering/CBGPU-backend).
[CBGPU-platform](https://github.com/CBGPU-Engineering/CBGPU-platform) assembles this repository at `MaterialShare/`.

CI starts a real loopback Uvicorn server with a synthetic upstream to verify
readiness and unauthenticated file-access rejection. It also checks imports and
syntax; live file transfer,
RLS and quota integration checks require an isolated backend and test storage.
See [CONTRIBUTING.md](CONTRIBUTING.md), [SECURITY.md](SECURITY.md) and
[LICENSE](LICENSE). Historical deployment notes are retained in docs for provenance;
old Funnel addresses are not the current default API entry point.

## Rust transfer implementation

The Axum implementation is a migration candidate; the Python service remains
the production writer until a drained handoff. Both versions use the same UUID
file layout, metadata API, caller authentication and PostgreSQL row policies.
No service-role key is needed.

Run isolated checks with Rust 1.88 or later:

```bash
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
docker build -f Dockerfile.rust -t cbgpu-materials-rust .
```

The default listener is 127.0.0.1:8423. Set CBGPU_MATERIALS_DIR to an existing
isolated directory and use the same metadata URL and public key variables as
the legacy service. Writes are disabled by default. A single Rust process
locks its storage directory; this lock does not coordinate with Python.
Before enabling CBGPU_MATERIALS_WRITES_ENABLED=1 and
CBGPU_MATERIALS_LEGACY_QUIESCED=1, route away and drain every legacy upload.
Do not run both writers against one directory. The container uses UID 65532;
verify directory and existing file access before handoff without changing
permissions during active uploads.

Tests use temporary files and a loopback metadata fixture. They cover chunk
resume, ranges, account isolation, lost completion acknowledgements, rollback
on request cancellation, persistent reservations, concurrent quota allocation
and symlink rejection. They never connect to production accounts or files.
