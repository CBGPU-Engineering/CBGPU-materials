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
