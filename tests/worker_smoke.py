"""Offline contract test of the real worker process (no external store requests)."""
import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import secrets
import socket
import subprocess
import tempfile
import threading
import time
from urllib.error import HTTPError
from urllib.request import Request, urlopen


class Fixture(BaseHTTPRequestHandler):
    def do_GET(self):
        body = json.dumps({'id': 42, 'link': 'https://example.invalid/homebrew',
                           'title': {'rendered': 'Worker Test Homebrew'},
                           'content': {'rendered': ''}}).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('binary', type=Path)
    args = parser.parse_args()
    fixture = ThreadingHTTPServer(('127.0.0.1', 0), Fixture)
    threading.Thread(target=fixture.serve_forever, daemon=True).start()
    with tempfile.TemporaryDirectory(prefix='iso2god-worker-test-') as directory:
        root = Path(directory)
        config = root / 'iso2god'
        config.mkdir()
        (config / 'app-settings.txt').write_text(
            f'store_base_url=http://127.0.0.1:{fixture.server_port}\n')
        (config / 'store-catalog.json').write_text(json.dumps({
            'fetched_at': int(time.time()), 'entries': [{
                'id': 42, 'title': 'Worker Test Homebrew',
                'page_url': 'https://example.invalid/homebrew'}]}))
        with socket.socket() as probe:
            probe.bind(('127.0.0.1', 0))
            port = probe.getsockname()[1]
        token = secrets.token_hex(32)
        environment = dict(os.environ, XDG_CONFIG_HOME=str(root), APPDATA=str(root),
                           ISO2GOD_WORKER_BIND=f'127.0.0.1:{port}', ISO2GOD_WORKER_TOKEN=token)
        with (root / 'worker.log').open('w') as log:
            process = subprocess.Popen([str(args.binary.resolve()), '--store-worker-service'],
                env=environment, stdin=subprocess.DEVNULL, stdout=log, stderr=log,
                creationflags=getattr(subprocess, 'CREATE_NO_WINDOW', 0))
            def request(path, body=None, authorization=token):
                headers = {'Content-Type': 'application/json'}
                if authorization is not None:
                    headers['Authorization'] = 'Bearer ' + authorization
                data = None if body is None else json.dumps(body).encode()
                req = Request(f'http://127.0.0.1:{port}{path}', data=data, headers=headers)
                try:
                    with urlopen(req, timeout=10) as response:
                        return response.status, json.load(response)
                except HTTPError as error:
                    return error.code, json.load(error)
            try:
                for _ in range(50):
                    try:
                        assert request('/health')[0] == 200
                        break
                    except OSError:
                        if process.poll() is not None:
                            raise RuntimeError('worker exited before becoming ready')
                        time.sleep(0.1)
                else:
                    raise RuntimeError('worker did not become ready')
                assert request('/v1/store/search', {'query': 'Worker'}, None)[0] == 401
                assert request('/v1/store/search', {'query': 'Worker'}, 'incorrect')[0] == 401
                assert request('/v1/store/search', {'query': ''})[0] == 400
                status, result = request('/v1/store/search', {'query': 'Worker'})
                assert status == 200 and len(result['listings']) == 1
                listing = result['listings'][0]
                assert listing['title'] == 'Worker Test Homebrew'
                status, result = request('/v1/store/inspect', {'listing': listing})
                assert status == 200 and result['listing']['id'] == '42'
                assert request('/v1/not-a-route')[0] == 404
                assert request('/v1/store/search', {'query': 'x' * (1024 * 1024)})[0] == 400
                print('PASS: health, missing/wrong auth, validation, search, inspect, unknown route, body limit')
            finally:
                process.terminate()
                process.wait(timeout=10)
    fixture.shutdown()


if __name__ == '__main__':
    main()
