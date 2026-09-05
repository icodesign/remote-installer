#!/usr/bin/env python3
"""Opt-in real HTTPS smoke test. Requires Python 3, curl, and configured Tailscale.

Run each provider separately, without other Serve/Funnel sessions on this node.
The synthetic IPA exercises transport, not signing or device installation.
"""

import argparse
import hashlib
import html
import ipaddress
import json
import pathlib
import plistlib
import queue
import re
import signal
import socket
import subprocess
import tempfile
import threading
import time
import urllib.parse
import zipfile


def config(binary, provider):
    mode = "funnel" if provider == "tailscale-funnel" else "serve"
    return json.loads(subprocess.check_output(
        [binary, mode, "status", "--json"], text=True, timeout=15)) or {}


def fixture(path):
    with zipfile.ZipFile(path, "w") as archive:
        archive.writestr("Payload/Probe.app/Info.plist", plistlib.dumps({
            "CFBundleIdentifier": "com.example.remote-installer.network-probe",
            "CFBundleVersion": "1", "CFBundleName": "Network Probe",
        }))
        archive.writestr("Payload/Probe.app/probe-data", bytes(range(256)) * 8192)
    return path.read_bytes()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--provider", required=True,
                        choices=["tailscale-serve", "tailscale-funnel"])
    parser.add_argument("--binary", default="target/debug/remote-installer")
    parser.add_argument("--tailscale-bin", default="tailscale")
    parser.add_argument("--port", type=int, default=443)
    parser.add_argument("--resolve-address", help="Explicit destination IP; TLS hostname validation remains enabled. Use a public Funnel relay IP to test the public route from within a tailnet.")
    args = parser.parse_args()
    before = config(args.tailscale_bin, args.provider)
    if before not in ({}, None):
        raise RuntimeError("Existing Serve/Funnel config; refusing to run the live test")

    with tempfile.TemporaryDirectory(prefix="remote-installer-tailscale-") as directory:
        root = pathlib.Path(directory)
        ipa = root / "NetworkProbe.ipa"
        original = fixture(ipa)
        command = [args.binary, "share", str(ipa), "--provider", args.provider,
                   "--https-port", str(args.port), "--tailscale-bin", args.tailscale_bin,
                   "--allow-unsigned", "--no-qr", "--timeout", "240"]
        process = subprocess.Popen(command, stdout=subprocess.PIPE,
                                   stderr=subprocess.STDOUT, text=True, bufsize=1)
        lines = queue.Queue()

        def read_output():
            for line in process.stdout:
                print(line, end="", flush=True)
                lines.put(line.strip())
            lines.put(None)

        reader = threading.Thread(target=read_output, daemon=True)
        reader.start()
        try:
            deadline = time.monotonic() + 90
            page_url = None
            while time.monotonic() < deadline:
                try:
                    line = lines.get(timeout=1)
                except queue.Empty:
                    continue
                if line is None:
                    raise RuntimeError("Share exited before printing an install page")
                if line.startswith("Install page: "):
                    page_url = line.removeprefix("Install page: ")
                    break
            if page_url is None:
                raise RuntimeError("Startup timed out; complete any Tailscale setup shown above")
            parsed = urllib.parse.urlsplit(page_url)
            origin = f"{parsed.scheme}://{parsed.netloc}"
            assert parsed.scheme == "https", page_url
            if args.provider == "tailscale-serve" and not args.resolve_address:
                node = json.loads(subprocess.check_output(
                    [args.tailscale_bin, "status", "--json"], text=True, timeout=15))
                expected = set(node["Self"]["TailscaleIPs"])
                addresses = {item[4][0] for item in socket.getaddrinfo(
                    parsed.hostname, parsed.port or 443, type=socket.SOCK_STREAM)}
                if not addresses.intersection(expected):
                    raise RuntimeError(
                        f"Local DNS resolves {parsed.hostname} to {sorted(addresses)}, "
                        f"not this node's tailnet IPs {sorted(expected)}. "
                        "Check Use Tailscale DNS on this computer (tailscale set --accept-dns=true). "
                        "To isolate DNS during testing, pass --resolve-address with this node's tailnet IP. "
                        "This does not establish whether the phone's DNS is working.")
            if args.provider == "tailscale-funnel":
                address = args.resolve_address or socket.gethostbyname(parsed.hostname)
                if not ipaddress.ip_address(address).is_global:
                    raise RuntimeError("Funnel must be tested through its PUBLIC relay, not the tailnet address. Pass --resolve-address with the hostname's public DNS address.")
                args.resolve_address = address
                print(f"Testing public Funnel relay {address}", flush=True)

            def fetch(url, *, byte_range=None):
                # Manifest URLs must stay on the same HTTPS endpoint.
                assert url.startswith(origin + "/"), url
                cmd = ["curl", "--silent", "--show-error", "--noproxy", "*",
                       # First TLS handshake can provision the node's certificate.
                       "--connect-timeout", "45", "--max-time", "60",
                       "--output", str(root / "body"), "--dump-header", str(root / "headers"),
                       "--write-out", "%{http_code}"]
                if args.resolve_address:
                    cmd += ["--resolve", f"{parsed.hostname}:{parsed.port or 443}:{args.resolve_address}"]
                if byte_range:
                    cmd += ["--range", byte_range]
                result = subprocess.run(cmd + [url], capture_output=True, text=True, timeout=65)
                if result.returncode:
                    raise RuntimeError(result.stderr.strip())
                return int(result.stdout), (root / "body").read_bytes(), (root / "headers").read_text()

            status, body, _ = fetch(page_url)
            assert status == 200, (status, body[:200])
            action = html.unescape(re.search(r'href="(itms-services:[^"]+)"', body.decode()).group(1))
            manifest_url = urllib.parse.parse_qs(urllib.parse.urlsplit(action).query)["url"][0]
            status, body, _ = fetch(manifest_url)
            assert status == 200, status
            manifest = plistlib.loads(body)
            assets = manifest["items"][0]["assets"]
            download_url = next(asset["url"] for asset in assets if asset["kind"] == "software-package")
            status, body, headers = fetch(download_url, byte_range="0-1023")
            assert status == 206, (status, headers)
            assert body == original[:1024], "Range response differs from fixture"
            assert f"content-range: bytes 0-1023/{len(original)}" in headers.lower(), headers
            status, body, _ = fetch(download_url)
            assert status == 200, status
            assert hashlib.sha256(body).digest() == hashlib.sha256(original).digest(), "Full IPA SHA-256 mismatch"
            print(f"PASS {args.provider}: trusted HTTPS, install page, manifest, Range, full IPA SHA-256", flush=True)
        finally:
            if process.poll() is None:
                process.send_signal(signal.SIGINT)
                try:
                    process.wait(timeout=20)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
                    raise RuntimeError("Share failed to stop; inspect Tailscale configuration")
            reader.join(timeout=2)
            # The daemon removes foreground state after the child's bus
            # connection closes; allow that asynchronous cleanup to finish.
            deadline = time.monotonic() + 5
            after = config(args.tailscale_bin, args.provider)
            while after != before and time.monotonic() < deadline:
                time.sleep(0.2)
                after = config(args.tailscale_bin, args.provider)
            assert after == before, f"Tailscale configuration was not restored: {after}"
            print("PASS cleanup: Tailscale configuration restored", flush=True)
        assert process.returncode == 0, process.returncode


if __name__ == "__main__":
    main()
