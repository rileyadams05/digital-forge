"""End-to-end background bridge smoke test. Uses synthetic files, never a real Xbox."""
import json
import os
from pathlib import Path
import queue
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import zipfile

binary = str(Path(sys.argv[1]).resolve())
root = Path(tempfile.mkdtemp(prefix="iso2god-bridge-smoke-")).resolve()
environment = dict(os.environ, APPDATA=str(root / "config"),
                   XDG_CONFIG_HOME=str(root / "config"), ISO2GOD_DISABLE_UPDATE_CHECK="1")
process = subprocess.Popen([binary, "--mcp-server"], stdin=subprocess.PIPE,
                           stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                           text=True, encoding="utf-8", env=environment)
responses = queue.Queue()
def read_responses():
    for line in process.stdout:
        responses.put(line)
threading.Thread(target=read_responses, daemon=True).start()
sequence = 0
def rpc(method, params=None):
    global sequence
    sequence += 1
    process.stdin.write(json.dumps({"jsonrpc": "2.0", "id": sequence,
                                   "method": method, "params": params or {}}) + "\n")
    process.stdin.flush()
    line = responses.get(timeout=30)
    response = json.loads(line)  # Any UI/progress text on stdout fails this test.
    assert response["id"] == sequence, response
    assert "error" not in response, response
    return response["result"]
def tool(name, arguments=None, error=False):
    result = rpc("tools/call", {"name": name, "arguments": arguments or {}})
    assert result["isError"] == error, result
    return result["structuredContent"]
def wait_job(job):
    deadline = time.monotonic() + 120
    while time.monotonic() < deadline:
        status = tool("job_status", {"job_id": job["job_id"]})
        if status["state"] != "running":
            return status
        time.sleep(0.15)
    raise AssertionError("worker did not finish")
def start(arguments):
    return tool("start_job", arguments)
try:
    rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                       "clientInfo": {"name": "smoke", "version": "1"}})
    tools = rpc("tools/list")["tools"]
    assert len(tools) == 6, tools
    tool("converter_status")
    usb = rpc("tools/call", {"name": "list_usb_drives", "arguments": {}})
    if usb["isError"]:
        # WSL1 has no block-device sysfs. Keep this opt-in; ordinary builds must enumerate.
        assert os.environ.get("ISO2GOD_SMOKE_ALLOW_NO_SYSFS") == "1", usb
        assert "/sys/dev/block" in usb["structuredContent"]["error"], usb
        print("SKIP USB discovery: this Linux environment has no block-device sysfs")
    tool("start_job", {"action": "execute_shell"}, error=True)
    tool("start_job", {"action": "process_games"}, error=True)
    tool("start_job", {"action": "check_updates", "password": "not-for-this-action"}, error=True)

    source = root / "source" / "Example Game"
    source.mkdir(parents=True)
    # Minimal structurally valid XEX header with a synthetic title ID. The
    # smoke test exercises the prepared-game path without using copyrighted
    # game data.
    xex = bytearray(52)
    xex[0:4] = b"XEX2"
    xex[20:24] = (1).to_bytes(4, "big")
    xex[24:28] = (0x00040006).to_bytes(4, "big")
    xex[28:32] = (32).to_bytes(4, "big")
    xex[32:36] = (0x11223344).to_bytes(4, "big")
    xex[36:40] = (2).to_bytes(4, "big")
    xex[40:44] = (2).to_bytes(4, "big")
    xex[44:48] = (0x12345678).to_bytes(4, "big")
    xex[50:52] = b"\x01\x01"
    (source / "default.xex").write_bytes(xex)
    (source / "assets").mkdir()
    (source / "assets" / "data.bin").write_bytes(bytes(range(256)) * 32)
    output = root / "output"
    output.mkdir()
    plan = {"action": "process_games", "sources": [str(source)],
            "destination": "local", "destination_path": str(output)}
    completed = wait_job(start(plan))
    assert completed["state"] == "completed", completed
    prepared_output = output / "Content/0000000000000000/12345678"
    assert (prepared_output / "assets/data.bin").read_bytes() == (source / "assets/data.bin").read_bytes()
    assert source.is_dir()
    assert "Copying game to local storage" in completed["progress"]
    assert wait_job(start(plan))["state"] == "failed"  # Existing output is not overwritten.
    verified = wait_job(start({"action": "verify_game", "sources": [str(prepared_output)]}))
    assert verified["state"] == "completed", verified

    archive = root / "Single.zip"
    with zipfile.ZipFile(archive, "w") as stream:
        for path in source.rglob("*"):
            if path.is_file():
                stream.write(path, Path(source.name) / path.relative_to(source))
    tool("scan_game_folder", {"folder": str(root)})
    tool("inspect_game_input", {"path": str(archive)})
    seven_zip = shutil.which("7z") or shutil.which("7zz")
    if os.name == "nt" and not seven_zip:
        candidate = Path(os.environ.get("ProgramFiles", "C:/Program Files")) / "7-Zip/7z.exe"
        if candidate.exists():
            seven_zip = str(candidate)
    if seven_zip:
        archive_output = root / "archive-output"
        archive_output.mkdir()
        result = wait_job(start(dict(plan, sources=[str(archive)], destination_path=str(archive_output))))
        assert result["state"] == "completed", result
        assert archive.is_file()

        # A dropped parent folder must discover a single ZIP several levels
        # below it, then exhaust sibling branches and more than four wrappers.
        nested_drop = root / "nested-drop"
        nested_archive = nested_drop / "a" / "b" / "c" / "Deep.zip"
        nested_archive.parent.mkdir(parents=True)
        wrapped = archive
        for level in range(6):
            wrapper = root / f"wrapper-{level}.zip"
            with zipfile.ZipFile(wrapper, "w") as stream:
                stream.write(wrapped, f"level-{level}/inner.zip")
            wrapped = wrapper
        sidecar = root / "sidecar.zip"
        with zipfile.ZipFile(sidecar, "w") as stream:
            stream.writestr("readme.txt", "No game in this branch; continue searching.")
        with zipfile.ZipFile(nested_archive, "w") as stream:
            stream.write(sidecar, "first/notes.zip")
            stream.write(wrapped, "second/game.zip")
        nested_output = root / "nested-output"
        nested_output.mkdir()
        result = wait_job(start(dict(plan, sources=[str(nested_drop)],
                                    destination_path=str(nested_output))))
        assert result["state"] == "completed", result
        assert (nested_output / "Content/0000000000000000/12345678/assets/data.bin").read_bytes() == (source / "assets/data.bin").read_bytes()
        assert nested_archive.is_file()
        # The interactive progress panel redraws in place, so completed stage
        # messages are intentionally not retained as scrollback. The delivered
        # payload above proves that every wrapper was traversed successfully.

        # Reproduce the damaged ZIP's missing end directory with synthetic data.
        broken = root / "broken.zip"
        broken.write_bytes(archive.read_bytes()[:-22])
        broken_output = root / "broken-output"
        broken_output.mkdir()
        result = wait_job(start(dict(plan, sources=[str(broken)],
                                    destination_path=str(broken_output))))
        assert result["state"] == "failed", result
        assert "does not by itself mean a numbered part is missing" in str(result), result
        assert "7-Zip could not inspect" in str(result) and "exit code" in str(result), result
        assert broken.is_file()
        assert not list(broken_output.iterdir())

        bad_payload = root / "bad-payload.zip"
        bad_bytes = bytearray(archive.read_bytes())
        with zipfile.ZipFile(archive) as stream:
            first_entry = stream.infolist()[0]
            offset = first_entry.header_offset
            name_length = int.from_bytes(bad_bytes[offset + 26:offset + 28], "little")
            extra_length = int.from_bytes(bad_bytes[offset + 28:offset + 30], "little")
            bad_bytes[offset + 30 + name_length + extra_length] ^= 0xFF
        bad_payload.write_bytes(bad_bytes)
        result = wait_job(start(dict(plan, sources=[str(bad_payload)],
                                    destination_path=str(broken_output))))
        assert result["state"] == "failed" and "CRC Failed" in str(result), result
        assert bad_payload.is_file()
        assert not list(broken_output.iterdir())

        # Finding a game in one branch must not conceal a broken sibling archive.
        broken_branch = root / "broken-branch.zip"
        with zipfile.ZipFile(broken_branch, "w") as stream:
            stream.write(archive, "a-game.zip")
            stream.write(broken, "z-broken.zip")
        result = wait_job(start(dict(plan, sources=[str(broken_branch)],
                                    destination_path=str(broken_output))))
        assert result["state"] == "failed" and "z-broken.zip" in str(result), result
        assert broken_branch.is_file()
        assert not list(broken_output.iterdir())

        # Optional real-world corrupt archive: verify the app reports its error
        # without modifying the source or producing a delivered game.
        if len(sys.argv) > 2:
            diagnostic_archive = Path(sys.argv[2]).resolve()
            original_stat = diagnostic_archive.stat()
            result = wait_job(start(dict(plan, sources=[str(diagnostic_archive)],
                                        destination_path=str(broken_output))))
            assert result["state"] == "failed", result
            assert "Headers Error" in str(result), result
            assert diagnostic_archive.stat().st_size == original_stat.st_size
            assert diagnostic_archive.stat().st_mtime_ns == original_stat.st_mtime_ns
            assert not list(broken_output.iterdir())
            print("PASS: supplied corrupt archive rejected with its header diagnostic; source unchanged.")

        parts = root / "parts"
        parts.mkdir()
        subprocess.run([seven_zip, "a", "-t7z", "-mx=0", "-v2k",
                        str(parts / "Split.7z"), str(source)], check=True,
                       stdout=subprocess.DEVNULL)
        first = parts / "Split.7z.001"
        assert first.is_file()
        split_output = root / "split-output"
        split_output.mkdir()
        result = wait_job(start(dict(plan, sources=[str(first)], destination_path=str(split_output))))
        assert result["state"] == "completed", result
        assert first.is_file(), "default must preserve original volumes"
        delete_output = root / "delete-output"
        delete_output.mkdir()
        result = wait_job(start(dict(plan, sources=[str(first)], destination_path=str(delete_output),
                                     remove_archive_parts=True)))
        assert result["state"] == "completed", result
        assert not first.exists(), "explicit deletion should happen only after success"
        assert source.is_dir()
    else:
        print("SKIP archive jobs: install 7-Zip to include extraction tests")

    updates = wait_job(start({"action": "check_updates"}))
    assert updates["state"] == "completed", updates
    assert updates["result"]["output"]["availableVersion"] is None
    tool("job_status", {"job_id": "../not-a-job"}, error=True)
    rpc("ping")
    print("PASS: MCP framing, local delivery, overwrite protection, recursive folders, deep/sibling archives, corrupt header/payload rejection, multipart preservation/cleanup, update check.")
finally:
    process.stdin.close()
    try:
        process.wait(timeout=15)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=10)
    if process.returncode:
        print(process.stderr.read(), file=sys.stderr)
    # Only the known dedicated test directory is removed.
    assert root.parent == Path(tempfile.gettempdir()).resolve()
    assert root.name.startswith("iso2god-bridge-smoke-")
    shutil.rmtree(root)
