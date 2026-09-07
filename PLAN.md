# Rust DHCP Server Implementation Plan

## 1. Purpose and Scope

Develop a Rust DHCPv4 server with static MAC-based assignments, BIOS/UEFI/iPXE
network boot settings, route delivery, a refreshable client CSV, and HTTP
endpoints `/health` and `/metrics`.

The server operates behind external DHCP relays: relays forward requests to the
server, and the application returns client settings. The application is not
itself a relay. There is no dynamic address pool. Unknown MAC addresses receive
no assignment.

TFTP and boot-file HTTP servers are external services. The application tells the
client which server and boot file to use.

The application supports Linux amd64 and arm64 bootloader profiles. Automated
binary, container, CI, and release builds currently target amd64 only.

## 2. Networking and DHCP

- Bind the UDP socket exactly to the configured `listen_ip`, on port 67.
- Allow `0.0.0.0`, including for container deployments.
- Exit with a clear error if the socket cannot be bound.
- Listen for HTTP on the `ip:port` specified by `http.listen`; wildcard addresses
  are also allowed.
- Accept requests from any relay, without `allowed_relays` or validation that the
  client subnet matches the relay address.
- Return settings only when the MAC is present in the active CSV snapshot.
- For Ethernet clients, use the MAC from `chaddr` after validating the hardware
  type and address length. Option 61 does not replace the MAC lookup key.
- Send replies for requests with a nonzero `giaddr` to the relay at `giaddr:67`.
- Support direct unicast lease-renewal requests, including after an initial lease
  obtained through a relay.
- Process `DISCOVER`, `REQUEST`, `INFORM`, `RELEASE`, and `DECLINE`; produce
  `OFFER`, `ACK`, and `NAK` according to the request state.
- Ignore requests addressed to another DHCP server. Do not send a NAK for every
  unknown or inapplicable request.
- Handle relay option 82 correctly without using it as an access restriction.
- If option 82 alone does not fit in the response, omit it entirely and record
  a warning/error counter as specified by RFC 3046; still send the reply.
- Validate packet lengths, DHCP options, the magic cookie, and field values.
  Account for response-size limits and long route lists.

### DHCP Server Identifier

When binding to a specific IP, use it as the Server Identifier by default. Add an
optional `dhcp.server_identifier` for wildcard binds and containers with forwarded
traffic. It must be a concrete address reachable by clients for lease renewal.

When using a wildcard bind without an explicit identifier, use the concrete local
destination address of the received packet from socket metadata. Never send
`0.0.0.0` as the Server Identifier. Behind NAT, explicitly configure an address
reachable by clients and document it in the deployment example.

### Leases and Conflicts

Do not maintain a lease database, assignment history, or separate conflict state.
Include a lease duration in DHCP replies. Record assignment, RELEASE, and DECLINE
events only in logs and metrics.

The only persistent application state is the latest valid CSV. Assignment changes
take effect when an update is loaded without waiting for previously issued leases
to expire. IP uniqueness is checked within the new CSV; use of an address by a
client from an older version is not tracked.

## 3. Main Configuration

Use TOML. Read the main file at startup; hot reload of this file is outside the
first release. Only the client CSV is refreshed periodically.

```toml
[dhcp]
listen_ip = "0.0.0.0"
server_identifier = "192.0.2.10"
lease_seconds = 3600

[http]
listen = "0.0.0.0:8080"

[logging]
level = "info"
format = "text"
disable_timestamp = false
color = true

[client_options]
dns = ["10.10.0.53", "10.10.0.54"]
ntp = ["10.10.0.123"]
domain = "example.internal"
classless_routes = [
  "10.0.0.0/8",
  "172.16.0.0/12",
]

[boot]
tftp_server = "10.10.0.20"

[boot.architectures.x86_64]
bios_file = "undionly.kpxe"
uefi_file = "ipxe-x86_64.efi"
ipxe_file = "http://10.10.0.20/x86_64/boot.ipxe"

# Optional profile for future ARM64 clients.
[boot.architectures.arm64]
uefi_file = "ipxe-arm64.efi"
ipxe_file = "http://10.10.0.20/arm64/boot.ipxe"

[clients_source]
url = "https://config.example.internal/clients.csv"
poll_interval_seconds = 60
timeout_seconds = 10
state_file = "/var/lib/macmapd/clients.csv"
```

Validate addresses, positive intervals and lease durations, the logging filter and
format, destination networks, and boot parameter values. `classless_routes`
contains only networks; the next hop comes from the GW in each client's CSV
record. Reject a `/0` route and an empty specific-route list in this configuration.
The entire `[logging]` section is optional and defaults to `info`, JSON output,
timestamps enabled, and color enabled for text output. When the section is present,
all four fields are required.

## 4. Client CSV

The format does not require a header:

```text
location,hostname,boot_type,mac,ip,prefix_length,gateway
```

Example:

```csv
dc1,host.name1,uefi,AA:BB:CC:DD:EE:FF,1.2.3.4,24,1.2.3.1
dc1,host.name2,bios,AA:BB:CC:DD:EE:AA,1.2.3.5,24,1.2.3.1
```

Validation rules:

- Require exactly seven fields and valid location, MAC, IPv4, hostname, and
  prefix length values.
- Normalize MAC addresses for case-insensitive lookup.
- Accept `bios` or `uefi` for `boot_type`.
- Reject the entire file if a MAC is duplicated.
- Reject the entire file if an IP is assigned to more than one MAC.
- Allow duplicate hostnames.
- For conventional Ethernet subnets, require IP and GW to be in the same subnet;
  do not assign network or broadcast addresses to clients.
- Support `/1` through `/30` with conventional subnet semantics. Support `/31`
  for point-to-point links, where both addresses are usable and the client and
  gateway must be distinct. Reject `/32` for Ethernet assignments.
- Reject the entire update when any row is invalid, reporting the row and cause.

Store active data as an immutable snapshot indexed by MAC and IP. Each DHCP
request uses exactly one snapshot version.

## 5. Client Parameters

- IP: from the CSV; use `yiaddr` in replies that assign an address.
- Subnet mask: from the CSV prefix, option 1.
- Hostname: from the CSV, option 12.
- DNS: from the main configuration, option 6.
- NTP: from the main configuration, option 42.
- Domain: from the main configuration, option 15.
- GW: from the CSV, option 3 when default-route mode is selected.
- Specific routes: option 121 when the corresponding route mode is selected.
- Boot parameters: based on the current boot stage and architecture.
- Lease duration and Server Identifier: in applicable DHCP replies.

For `INFORM`, return parameters without assigning a new lease. Construct the
fields and options in other replies according to the DHCP message type.

## 6. Boot Stage and Architecture Detection

The machine type in the CSV is not the current request stage: after UEFI or BIOS,
the same MAC may request settings from iPXE and later from the OS.

Classification order:

1. Detect iPXE from user class option 77, supporting the representation actually
   used by iPXE.
2. Detect PXE requests from client signals, including vendor class and
   architecture.
3. For PXE, select BIOS or UEFI from the CSV; log conflicts with the detected mode.
4. Determine architecture from option 93, accounting for lists of values and
   compatibility with common UEFI x86-64 implementations. Fall back to the
   vendor-class `Arch:` value for both PXE and iPXE when option 93 is unavailable.
5. Use the `x86_64` profile for legacy BIOS. This identifies the boot-file profile,
   not necessarily the CPU bitness.
6. If no bootloader signals are present, classify the request as `os`. Do not
   promise identification of a particular operating system from DHCP.
7. Use `unknown` for malformed or ambiguous requests and preserve the diagnostic
   reason.

Do not retain boot-stage history. If architecture cannot be determined, report it
as `unknown`. If no suitable file exists, return network settings without a boot
file and log the reason.

For an initial PXE stage, select `bios_file` or `uefi_file` from the CSV
`boot_type`, while logging any conflict with the mode detected from the request.
For iPXE, return the architecture-specific `ipxe_file`. This prevents sending the
iPXE binary again during the iPXE stage. Do not return boot parameters to a
regular OS.

Support options 66/67 and BOOTP fields `siaddr`/`file`; test compatibility and
name/URL length limits with real bootloaders.

## 7. Route Policy

Determine whether option 121 was requested from Parameter Request List option 55.

| Current client | Requested option 121 | Reply route options |
|---|---|---|
| BIOS/UEFI PXE | Yes or no | Option 3 with the client GW; no 121 |
| iPXE | No | Option 3 with the client GW; no 121 |
| iPXE | Yes | Option 121 with specific routes; no option 3 |
| OS | No | Option 3 with the client GW; no 121 |
| OS | Yes | Option 121 with specific routes; no option 3 |

For a request whose stage cannot be determined reliably, apply the PRL option 121
rule if the packet is otherwise valid. Reject malformed packets.

In specific-route mode, every route uses the client's GW, option 3 is absent, and
`0.0.0.0/0` is not added. Other settings such as DNS and NTP are still returned.

DNS, NTP, and boot resources used by iPXE must be reachable through the connected
network or configured specific routes when the client receives option 121.

## 8. Polling and Local State

Startup and refresh algorithm:

1. Read and validate the main TOML file.
2. Load and validate the saved CSV if it exists.
3. Start the HTTP and DHCP handlers; do not assign addresses without a valid
   snapshot.
4. Fetch the remote CSV immediately, then repeat at the configured interval.
5. Download the complete new version with a timeout and response-size limit.
6. Validate the entire CSV and build a new snapshot.
7. Persist it safely using a temporary file in the same directory, synchronization,
   and atomic rename.
8. Atomically replace the active in-memory snapshot.

Requests continue using the previous snapshot until an update is complete. A
network error, HTTP error, invalid CSV, or persistence failure does not alter the
active data. Do not activate a new snapshot if writing it fails.

If no usable state exists at startup and the source is unavailable, keep retrying;
`/health` returns 503 and DHCP does not assign addresses.

Use valid stale state without automatic expiration. Support `ETag` and
`Last-Modified`, and do not overlap refresh operations. An unchanged response does
not require rewriting the file. Report state read and write failures in logs and
metrics.

Only HTTP 200 can replace the snapshot; accept 304 only with a downloaded cache.
Reject all other statuses, including 204 and 206, without altering memory or disk.
An empty/header-only CSV in a 200 response intentionally clears assignments and
remains a valid snapshot for health checks.

## 9. HTTP, Logs, and Metrics

### GET /health

- HTTP 200: the DHCP handler is operational and a valid client snapshot is loaded.
- HTTP 503: service is unavailable, particularly before any valid snapshot exists.
- The JSON contains the active data source, client count, time of the latest
  successful refresh, data age, and latest synchronization error.
- An unavailable remote source does not make the service unhealthy while a usable
  local snapshot exists.
- After restart, do not report the state-file read time as the time of a successful
  network fetch; represent unknown timestamps explicitly.
- Include `Server: macmapd/<version>` and `X-App-Version: <version>` headers.

### GET /metrics

Use Prometheus text format. Every metric name starts with `macmapd_`. The exposed
series are:

- `macmapd_build_info{version}`.
- `macmapd_requests_total`, `macmapd_responses_total`,
  `macmapd_errors_total`, and `macmapd_unknown_clients_total`.
- `macmapd_sync_success_total` and `macmapd_sync_errors_total`.
- `macmapd_state_read_errors_total` and `macmapd_state_write_errors_total`.
- `macmapd_clients`.
- `macmapd_last_successful_sync_timestamp_seconds` and
  `macmapd_data_age_seconds`.
- `macmapd_client_requests_total{location,hostname,stage,route,message}`.
- `macmapd_message_responses_total{message}`.
- `macmapd_response_duration_seconds_count` and
  `macmapd_response_duration_seconds_sum`.

Malformed packets increment the general error counter and use fixed `unknown`
client labels with `message="invalid"`; detailed failure reasons remain in logs.
RELEASE and DECLINE are visible through the `message` label on client requests.

Hostname and location are permitted as labels on client metrics and come from the
CSV. Identical hostnames are aggregated when all other labels match. For unknown
MAC addresses, use fixed `hostname="unknown"` and `location="unknown"` labels,
not a name supplied by the request. MAC is not required as a label.

Expire client-label series after 24 hours of inactivity, checking at most once
per minute on requests or scrapes. Keep aggregate counters unchanged. Copy the
series under the mutex and format the response after releasing it.

### Logging

Logging is configured by level, `json` or `text` format, optional timestamps, and
ANSI color for text output. The optional `[logging]` section uses the defaults
described in section 3. Write a log entry for every request, including its boot
stage:

```text
mac, location, hostname, dhcp_message, xid, relay_ip,
boot_stage, architecture, assigned_ip,
route_mode, boot_file, result
```

`boot_stage` is one of `bios`, `uefi`, `ipxe`, `os`, or `unknown`. Parsed requests
include the listed context, with optional response fields represented explicitly.
If parsing fails before request fields can be trusted, log the parsing error with
`boot_stage="unknown"` and `result="malformed"`. Also log startup, socket binding,
CSV updates, state errors, and shutdown. Send logs to stdout/stderr for systemd
and containers.

Warnings and errors carry client context directly, even when INFO spans are
disabled. This includes boot-mode mismatches and unavailable boot files.

## 10. Application Structure

| Component | Responsibility |
|---|---|
| `config` | TOML, validation, and `check-config` |
| `clients` | CSV, indexes, and immutable snapshots |
| `dhcp` | UDP, packet parsing, boot/route policy, protocol handling, and replies |
| `runtime` | HTTP polling, atomic state persistence, health, and metrics |
| `main` | CLI parsing, logging initialization, task orchestration, and shutdown |

There is no lease-storage component. DHCP processing is independent of HTTP
polling and disk operations. Implement option selection as testable functions of
the request, client record, and main configuration.

The protocol implementation directly handles unknown options, options 82 and 93,
long option 121 payloads, client packet-size limits, and PXE fields. Dependency
versions are locked in `Cargo.lock`, while the Rust version is pinned in
`rust-toolchain.toml`.

## 11. Containers and Builds

- Use a multi-stage Dockerfile with a Rust build stage and a distroless Debian 13
  non-root runtime image.
- Use Podman as the default engine for generic container recipes, while retaining
  explicit Docker/Buildx recipes.
- Build the `x86_64-unknown-linux-gnu` release archive with `cargo-dist` inside a
  temporary Linux container.
- Automated Docker Buildx image builds target `linux/amd64` only.
- Use the distroless image's glibc and CA certificates for the dynamically linked
  binary and HTTPS polling.
- Mount the main configuration as a separate read-only file.
- Mount the state directory as a writable volume accessible to the process user.
- Add `.dockerignore`.
- Document UDP/67 and HTTP port publication, privileges for binding port 67 and
  writing state, and Server Identifier configuration behind NAT.
- Use `listen_ip = "0.0.0.0"` in the container configuration example.
- Verify image startup, HTTP, and CSV persistence/recovery through a volume in the
  smoke test.
- Publish to a registry only through a separate, explicitly invoked recipe.

### GitHub Actions

- Run formatting, Clippy, unit tests, integration tests, and a release build on
  every push and pull request across all branches.
- On pushes to `main`, build and publish the `linux/amd64`
  `ghcr.io/<owner>/macmapd:latest` image.
- On `vX.Y.Z` tags reachable from `main`, publish `stable`, `vX.Y.Z`, and `X.Y.Z`
  amd64 image tags and create a GitHub Release containing the amd64 cargo-dist
  archive and its SHA-256 checksum file.
- Use the repository `GITHUB_TOKEN` with `packages: write` and `contents: write`
  permissions for publishing jobs.

Both publishing workflows depend on reusable CI, including the real-container
smoke test. Require stable tags to match Cargo.toml and Cargo.lock, and pass the
explicit release tag to cargo-dist. Build archives and smoke-test the exact image
before pushing it; fail when expected release files are missing. Publishing to
GHCR and GitHub is not an atomic transaction.

## 12. Justfile

| Recipe | Purpose |
|---|---|
| `just build` | Debug build for the current platform |
| `just build-release` | Release build for the current platform |
| `just build-amd64` | x86-64 Linux binary |
| `just dist-plan` | Show the planned cargo-dist artifacts |
| `just dist-build` | Build the amd64 cargo-dist release archive |
| `just fmt` | Format source code |
| `just fmt-check` | Check formatting |
| `just lint` | Run Clippy with warnings treated as errors |
| `just test` | Run unit tests |
| `just test-integration` | Test DHCP, polling, and state recovery |
| `just test-release` | Validate release-version checks with Python 3.11+ |
| `just check` | Run fmt-check, lint, Rust unit tests, and release-validation tests |
| `just container-build` | Build the amd64 image with the configured engine |
| `just container-smoke IMAGE` | Smoke-test with the configured engine |
| `just docker-build-amd64` | Build a local amd64 image |
| `just docker-push` | Publish a tagged amd64 image |
| `just docker-smoke IMAGE` | Smoke-test an image with Docker |
| `just podman-build-amd64` | Build a local amd64 image with Podman |

Parameterize the image name, tag, artifact directory, and generic container engine.
Document Docker/Buildx requirements. Build commands use the lockfile. Document Linux capabilities and
privileges separately for network integration tests.

## 13. Testing

### Unit and Package Tests

- TOML/CSV: valid data, duplicate MAC/IP, allowed duplicate hostnames, and malformed
  rows.
- Route matrix for BIOS, UEFI, iPXE, and OS with and without option 121 requests.
- Route encoding with different prefix lengths and client gateways.
- Boot-file selection and detection of iPXE, UEFI x86-64, and ARM64.
- Malformed packets, unknown options, and response-size limits.
- REQUEST in different states, requests to another server, INFORM, RELEASE, and
  DECLINE.
- Snapshot replacement without mixing versions in a single reply.

### Integration Tests

- Isolated Linux network: client -> relay -> server.
- Multiple relays without prior enumeration in the configuration.
- Binding to a specific IP and to a wildcard address.
- Initial assignment and direct unicast renewal.
- Actual options and reply destination verified from packet captures.
- Unavailable source, invalid CSV, write failure, HTTP 304, and restart recovery.
- Health/metrics with usable state and without data.
- BIOS/UEFI/iPXE in a virtual environment and, where available, on real clients.
- amd64 builds and container smoke tests.

## 14. Implementation Stages

1. Create the Rust project skeleton, pinned toolchain, TOML/CSV parsing, validation,
   `check-config` command, examples, and basic justfile.
2. Implement DHCP UDP transport, binding, Server Identifier, relay operation,
   static assignments, and required message types without lease storage.
3. Implement network options, BIOS/UEFI/iPXE architecture profiles, routes, and
   boot-stage logging.
4. Implement polling, atomic state persistence, recovery, and snapshot replacement.
5. Implement HTTP health/metrics, operational logs, and graceful shutdown.
6. Add the Dockerfile, amd64 build recipes, deployment example,
   and systemd unit.
7. Run integration checks, document limitations, and verify acceptance criteria.

## 15. Acceptance Criteria

- Sockets listen on configured addresses, including an explicitly configured
  wildcard.
- Requests are accepted from any relay; a known MAC receives its CSV assignment.
- IP, mask, hostname, DNS, NTP, and domain match the configuration.
- Duplicate IP/MAC values reject the CSV; duplicate hostnames are allowed.
- BIOS/UEFI receive a default route and appropriate boot file.
- iPXE and OS clients requesting option 121 receive specific routes without a
  default route or option 3.
- Every request logs a detected boot stage, or `unknown` with a reason.
- Lease renewal works without a lease or conflict database.
- A refresh error does not change active data; saved CSV state supports restart
  while the source is unavailable.
- Health and metrics reflect service state; hostname and location are supported in
  labels, and all metric names use the `macmapd_` prefix.
- `/health` and `/metrics` expose the application version in HTTP headers, and
  metrics include `macmapd_build_info` with a version label.
- `just check`, integration tests, and the automated amd64 build pass.
- The container image starts with mounted configuration/state and passes the smoke
  test.

## 16. Protocol References

- [RFC 2131 — DHCPv4](https://www.rfc-editor.org/rfc/rfc2131.html)
- [RFC 2132 — DHCP Options](https://www.rfc-editor.org/rfc/rfc2132.html)
- [RFC 3442 — Classless Static Routes](https://www.rfc-editor.org/rfc/rfc3442.html)
- [RFC 3046 — Relay Agent Information Option](https://www.rfc-editor.org/rfc/rfc3046.html)
- [RFC 4578 — PXE DHCP Options](https://www.rfc-editor.org/rfc/rfc4578.html)
- [iPXE — DHCP configuration](https://ipxe.org/howto/dhcpd)

During implementation, verify current architecture codes against the IANA registry
and test compatibility using packets from the target clients.
