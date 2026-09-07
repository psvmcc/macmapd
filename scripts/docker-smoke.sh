#!/bin/sh
# Requires Podman or Docker, curl and a built image. No host DHCP port is used.
set -eu
image=${1:?Usage: docker-smoke.sh IMAGE:TAG}
engine=${CONTAINER_ENGINE:-podman}
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
smoke_dir=$(mktemp -d)
smoke_name="macmapd-smoke-$$"
cleanup() {
    "$engine" rm -f "$smoke_name" "$smoke_name-source" >/dev/null 2>&1 || true
    "$engine" network rm "$smoke_name" >/dev/null 2>&1 || true
    "$engine" volume rm "$smoke_name-state" >/dev/null 2>&1 || true
    # Keep generated files for diagnostics; never remove an unresolved path.
    printf 'Smoke-test files: %s\n' "$smoke_dir"
}
trap cleanup EXIT HUP INT TERM
mkdir "$smoke_dir/source"
mkdir "$smoke_dir/state"
chmod 0777 "$smoke_dir/state"
printf '%s\n' 'smoke,smoke.example,uefi,AA:BB:CC:DD:EE:FF,10.20.0.10,24,10.20.0.1' > "$smoke_dir/source/clients.csv"
cat > "$smoke_dir/config.toml" <<EOF
[dhcp]
listen_ip = "0.0.0.0"
lease_seconds = 3600
[http]
listen = "0.0.0.0:8080"
[logging]
level = "warn"
format = "json"
disable_timestamp = true
color = false
[client_options]
dns = ["10.20.0.53"]
ntp = ["10.20.0.123"]
domain = "example"
classless_routes = ["10.0.0.0/8"]
[boot]
tftp_server = "10.20.0.20"
[boot.architectures.x86_64]
bios_file = "undionly.kpxe"
uefi_file = "ipxe.efi"
ipxe_file = "http://10.20.0.20/boot.ipxe"
[clients_source]
url = "http://$smoke_name-source/clients.csv"
poll_interval_seconds = 1
timeout_seconds = 2
state_file = "/var/lib/macmapd/clients.csv"
EOF
"$engine" network create "$smoke_name" >/dev/null
"$engine" run -d --name "$smoke_name-source" --network "$smoke_name" \
    --mount "type=bind,src=$smoke_dir/source,dst=/usr/share/nginx/html,readonly" docker.io/library/nginx:mainline-alpine >/dev/null
start_server() {
    "$engine" run -d --name "$smoke_name" --network "$smoke_name" \
        --cap-drop ALL --sysctl net.ipv4.ip_unprivileged_port_start=0 \
        -p 127.0.0.1::8080 \
        --mount "type=bind,src=$smoke_dir/config.toml,dst=/etc/macmapd/config.toml,readonly" \
        --mount "type=bind,src=$smoke_dir/state,dst=/var/lib/macmapd" "$image" >/dev/null
    smoke_port=$("$engine" port "$smoke_name" 8080/tcp | sed 's/.*://')
    attempt=0
    until curl --fail --silent "http://127.0.0.1:$smoke_port/health" >/dev/null; do
        attempt=$((attempt + 1))
        if [ "$attempt" -ge 30 ]; then
            "$engine" logs "$smoke_name"
            return 1
        fi
        sleep 1
    done
    curl --fail --silent "http://127.0.0.1:$smoke_port/metrics" >/dev/null
}
start_server
"$engine" run --rm --network "$smoke_name" \
    --mount "type=bind,src=$script_dir/relay-smoke.py,dst=/relay-smoke.py,readonly" \
    docker.io/library/python:3.14-alpine python /relay-smoke.py "$smoke_name"
"$engine" logs "$smoke_name" > "$smoke_dir/server.log" 2>&1
"$engine" run --rm --network none \
    --mount "type=bind,src=$smoke_dir/server.log,dst=/server.log,readonly" \
    docker.io/library/python:3.14-alpine python -c '
import json
events = [json.loads(line)["fields"] for line in open("/server.log") if line.strip()]
for key, value in (("result", "relay_option_omitted"), ("message", "boot mode mismatch"), ("message", "no matching boot file")):
    event = next(e for e in events if e.get(key) == value)
    assert event["location"] == "smoke", event
    assert event["hostname"] == "smoke.example", event
    assert event["mac"] == "aa:bb:cc:dd:ee:ff", event
    assert event["boot_stage"] in ("bios", "uefi"), event
print("PASS: WARN-level events retain client context")
'
test -s "$smoke_dir/state/clients.csv"
"$engine" stop "$smoke_name-source" >/dev/null
"$engine" rm -f "$smoke_name" >/dev/null
start_server
printf '%s\n' 'PASS: HTTP, CSV persistence and restart with unavailable source'
