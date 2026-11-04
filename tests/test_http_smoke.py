"""Real loopback Uvicorn smoke test; upstream metadata is a local fixture."""
import http.server
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]

class BackendFixture(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.end_headers()
        self.wfile.write(b'[]')

    def log_message(self, *_):
        pass

class ServerSmokeTests(unittest.TestCase):
    def test_readiness_and_unauthenticated_file_access(self):
        backend = http.server.ThreadingHTTPServer(('127.0.0.1', 0), BackendFixture)
        threading.Thread(target=backend.serve_forever, daemon=True).start()
        with socket.socket() as listener:
            listener.bind(('127.0.0.1', 0))
            port = listener.getsockname()[1]
        try:
            with tempfile.TemporaryDirectory() as storage:
                env = {key: os.environ[key] for key in ['PATH', 'SYSTEMROOT'] if key in os.environ}
                env.update(CBGPU_SUPABASE_URL=f'http://127.0.0.1:{backend.server_port}',
                           CBGPU_SUPABASE_ANON_KEY='synthetic-test-only',
                           CBGPU_MATERIALS_DIR=storage)
                process = subprocess.Popen([sys.executable, '-m', 'uvicorn', 'app:app', '--host',
                                            '127.0.0.1', '--port', str(port), '--log-level', 'error'],
                                           cwd=ROOT, env=env, stdout=subprocess.DEVNULL,
                                           stderr=subprocess.PIPE)
                base = f'http://127.0.0.1:{port}'
                try:
                    ready = False
                    for _ in range(100):
                        if process.poll() is not None:
                            self.fail(process.stderr.read().decode())
                        try:
                            with urllib.request.urlopen(base + '/health', timeout=1) as response:
                                ready = response.status == 200 and json.load(response)['ok']
                            if ready:
                                break
                        except (OSError, urllib.error.URLError):
                            time.sleep(0.05)
                    self.assertTrue(ready, 'Uvicorn did not become ready')
                    for route, method in [('/materials/00000000-0000-4000-8000-000000000001/content', 'GET'),
                                          ('/materials/00000000-0000-4000-8000-000000000001', 'DELETE')]:
                        with self.assertRaises(urllib.error.HTTPError) as failure:
                            urllib.request.urlopen(urllib.request.Request(base + route, method=method), timeout=3)
                        self.assertEqual(failure.exception.code, 401)
                        failure.exception.close()
                finally:
                    process.terminate()
                    process.wait(timeout=10)
                    process.stderr.close()
        finally:
            backend.shutdown()
            backend.server_close()

if __name__ == '__main__':
    unittest.main()
