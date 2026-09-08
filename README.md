# macdack

<p align="center">
  <img src="macdack.png" alt="macdack logo" width="420">
</p>

<p align="center">
  Logo artwork by <a href="https://github.com/greeddj">@greeddj</a> — thank you!
</p>

Static DHCPv4 provisioning driven by MAC-address mappings.

The name combines **MAC** and **DHCP ACK**. `macdack` provides deterministic IP,
routing, DNS, NTP, MTU, domain, and PXE/iPXE boot configuration for known
clients, with first-class support for DHCP relay deployments.

It is implemented in Rust and includes classless routes, periodically refreshed
CSV client data, health checks, and Prometheus metrics. Detailed requirements
are in [PLAN.md](PLAN.md).

## Running

You need the Rust version specified in `rust-toolchain.toml` (rustup installs it
automatically) and [just](https://github.com/casey/just). All builds use
`Cargo.lock`.
Release validation and `just check` also require Python 3.11 or newer.

```sh
just build-release
target/release/macdack check-config --config examples/server.toml
target/release/macdack --config examples/server.toml
```

Without arguments, `macdack` reads `/etc/macdack/config.toml`. Use `--config`
only for a different path. On Unix, `SIGHUP` validates the same TOML path and
replaces the current process in place, preserving its PID and reloading logging,
listeners, polling settings, saved state, and the remote CSV. Invalid TOML is
logged and the running process is retained.

Before starting the server, replace the example addresses, CSV URL, and state
path with your own values. On Linux, UDP/67 requires appropriate privileges. Use
the [examples/macdack.service](examples/macdack.service) unit, which grants
`CAP_NET_BIND_SERVICE` and creates `/var/lib/macdack`. The binary is installed at
`/usr/local/bin/macdack`, and the configuration at `/etc/macdack/config.toml`.

For Podman, [examples/macdack.container](examples/macdack.container) is an
equivalent Quadlet example. Place the file in `/etc/containers/systemd/`, then
run `systemctl daemon-reload` and
`systemctl start macdack.service`. Quadlet-generated services cannot be enabled
directly; the `[Install]` section makes the generator add the boot dependency.
Enable periodic registry checks with
`systemctl enable --now podman-auto-update.timer`. The Quadlet uses
`AutoUpdate=registry`, so Podman pulls a changed `stable` image and restarts the
generated service. `Pull=newer` also checks for a newer image whenever the
service starts. Registry authentication, when required, must be configured for
the root account running this system Quadlet.
The container uses host networking and a read-only root filesystem, drops every
capability, and restores only `CAP_NET_BIND_SERVICE`, which the non-root image
needs to bind DHCP port 67. The `/etc/macdack` host directory is mounted
read-only so atomic config-file replacement remains visible on SIGHUP, while a
bind-mounted host directory stores the cached CSV in `/var/lib/macdack`; create
that directory before starting the service. The `U` mount option makes it
writable by the image's non-root user and therefore changes its host ownership.
`systemctl reload macdack.service` sends SIGHUP to macdack.

The server accepts requests from any relay and looks clients up by the MAC in
`chaddr`. With a wildcard bind on Linux, the Server Identifier is derived from
the packet's local destination address. Other operating systems require an
explicit `dhcp.server_identifier` when using a wildcard bind. Always configure it
explicitly behind NAT: it must be an address reachable by clients, including for
direct lease renewal. The application is not itself a relay and does not serve
files over TFTP or HTTP.

Logging is configured in the `[logging]` section:

```toml
[logging]
level = "info"
format = "json" # json or text
disable_timestamp = false
color = true    # applies to text format
dhcp_packet_debug = false
```

`level` accepts standard `tracing_subscriber::EnvFilter` expressions, such as
`debug` or `macdack=debug,tower_http=warn`. Use `format = "text"` for readable
console output; `json` is generally more convenient for systemd and log
aggregators. Setting `disable_timestamp = true` omits the date and time. When
`dhcp_packet_debug = true`, received and sent packets are logged at DEBUG with
decoded fields and a tcpdump-like BOOTP/DHCP dump. The flag automatically
enables DEBUG events for the `macdack::dhcp` target even when the global `level`
is `info` or stricter. This is verbose and should normally be enabled only while
diagnosing DHCP traffic. XIDs use the tcpdump-compatible form `0x27e9542c`, and
the architecture field is named `arch`.

The `packet_dump` field includes BOOTP addresses and flags, MAC, boot fields,
XID, DHCP options, parameter request names, relay suboptions, and classless
routes. JSON logging escapes its embedded newlines; text logging renders it as a
readable multiline value. Since macdack uses a UDP socket rather than a raw
packet socket, IP ID, fragmentation flags, and other IP-header fields are not
available. Packet dumps require no additional Linux capability.

The CSV has nine fields and does not require a header (a header is also
supported): `location,hostname,domain,boot_type,mtu,mac,ip,prefix_length,gateway`.
Blank lines and lines beginning with `#` are ignored.

```csv
# Datacenter clients
dc1,host.example,example.internal,uefi,1500,AA:BB:CC:DD:EE:FF,10.20.0.10,24,10.20.0.1
dc1,host.example,example.internal,bios,9000,AA:BB:CC:DD:EE:AA,10.20.0.11,24,10.20.0.1
```

Duplicate MAC or IP values reject the entire update; duplicate hostnames are
allowed. `location` is included in logs and client metric labels so each
host's boot location is visible. Ethernet prefixes `/1` through `/30` use normal
subnet semantics; `/31` is supported for point-to-point links with distinct client
and gateway addresses, while `/32` is rejected. No lease database is maintained.
When reassigning addresses, the operator must account for leases that may still be
active for previous clients.

Domain and interface MTU are returned from each client row as DHCP options 15
and 26. MTU must be in the range 68..65535. `[client_options].timezone` defaults
to `Etc/UTC` and is returned as DHCP option 101 when requested by the client.

BIOS and UEFI clients receive a default gateway. iPXE and OS clients that request
option 121 receive only the specific routes through the GW in their CSV record,
without a default route. DNS, NTP, and boot resources must be reachable through
those routes. Boot file profiles are configured separately for x86_64 and arm64
and are independent of the server's architecture.
An initial BIOS request is ignored when its CSV row requires `uefi`, and an
initial UEFI request is ignored when the row requires `bios`. The mismatch is
logged without sending OFFER, ACK, or NAK. Later iPXE and OS requests remain
eligible for service.

`/health` returns 200 when the DHCP socket is ready and a valid client snapshot is
available; otherwise it returns 503. `/metrics` returns Prometheus text format;
hostname and location metric labels come from the CSV, and every metric name has
the `macdack_` prefix. Both endpoints include `Server: macdack/<version>` and
`X-App-Version: <version>` headers. The metrics payload also exposes
`macdack_build_info{version="..."}`. A valid saved CSV is used indefinitely while
the source is unavailable. The main TOML is reread after a valid Unix SIGHUP.

Only HTTP 200 replaces the CSV; a cached HTTP 304 keeps the current snapshot.
Other statuses, including 204 and 206, retain the previous data and report an
error. An empty or header-only CSV delivered with HTTP 200 intentionally clears
all assignments. A valid empty snapshot still satisfies `/health`.
Polling requests use `User-Agent: macdack/<version>`.
BIOS/UEFI mismatches increment `macdack_boot_mode_mismatches_total`.

Client metric series expire after 24 hours without requests (cleanup runs at
most once per minute during requests or scrapes). A returning series starts at
zero again; global request/response counters are not reset. Metric text is
formatted after releasing the client-series lock.

If relay option 82 alone would exceed the client's response-size limit, the
server omits it, logs a warning with client context, and increments
`macdack_errors_total`. Required network settings are not silently truncated.

## Containers and Release Artifacts

Podman is the default container engine for the generic recipes. Automated GitHub
Actions builds and publishes the `linux/amd64` image only. The application itself
still supports ARM64 boot profiles.

Use `just podman-build-amd64` and
`just container-smoke macdack:dev-amd64` for explicit Podman commands. The
generic `just container-build` recipe also uses Podman by default; set
`CONTAINER_ENGINE=docker` to select Docker instead. On macOS, Podman requires a
running machine, and building amd64 on ARM may require emulation support in that
VM. The smoke test performs a real UDP relay exchange and checks Linux wildcard
binding, UEFI/iPXE/OS routing, configuration reload, and saved-state recovery in
an isolated container network.

```sh
just docker-build-amd64
sudo install -d -m 0755 /etc/macdack
sudo install -m 0644 examples/server.toml /etc/macdack/config.toml
sudo install -d -m 0700 -o 65532 -g 65532 /var/lib/macdack
docker run -d --name macdack \
  --cap-drop ALL --sysctl net.ipv4.ip_unprivileged_port_start=0 \
  -p 67:67/udp -p 127.0.0.1:8080:8080 \
  --mount type=bind,src=/etc/macdack,dst=/etc/macdack,readonly \
  --mount type=bind,src=/var/lib/macdack,dst=/var/lib/macdack \
  macdack:dev-amd64
```

For containers, set `listen_ip = "0.0.0.0"`, HTTP
`listen = "0.0.0.0:8080"`, a client-reachable `server_identifier`, and the state
path `/var/lib/macdack/clients.csv`. The example uses a separate container network
namespace and permits binding low ports through sysctl. With host networking,
grant appropriate privileges for port 67 instead. Verify routing to the relay:
responses are sent to `giaddr:67`.

The runtime image is based on `gcr.io/distroless/cc-debian13:nonroot`. It has no
shell or package manager, but includes glibc and CA certificates needed by the
dynamically linked Rust binary and HTTPS CSV polling. The process runs as the
standard distroless UID/GID `65532:65532`. When bind-mounting the state directory,
make it writable by this UID. Linux binaries target the Debian Trixie glibc
environment or a compatible one.

```sh
just build-amd64                # cargo-dist artifact for x86_64-unknown-linux-gnu
just dist-plan                  # show planned cargo-dist artifacts
just dist-build                 # build the amd64 cargo-dist artifact
just container-build             # build an amd64 image with Podman by default
just container-smoke macdack:dev-amd64
just docker-push registry.example/dhcp v0.2.0
```

`build-amd64` runs `cargo-dist` in a temporary Linux container and places the
release archive in `target/distrib`. The image name, tag, and OCI artifact
directory are controlled by `IMAGE`, `TAG`, and `ARTIFACTS`.
Select Docker for generic recipes with `CONTAINER_ENGINE=docker`. Only
`docker-push` publishes an image and requires an explicit name and tag.

## GitHub Actions

The repository includes three workflows:

- `CI` runs checks, unit tests, integration tests, and a release build on every
  push and pull request for every branch, including an amd64 container smoke test
  against the real DHCP listener.
- `Publish main container` builds and publishes the `linux/amd64`
  `ghcr.io/<owner>/macdack:latest` image after every push to `main`.
- `Release` accepts `vX.Y.Z` tags that point to a commit reachable from `main`,
  publishes `stable`, `vX.Y.Z`, and `X.Y.Z` image tags, and attaches the amd64
  cargo-dist archive plus its SHA-256 file to the GitHub Release.

Publishing workflows require the reusable CI checks to pass. Release tags must
exactly match the application version in both `Cargo.toml` and `Cargo.lock`;
prerelease tags are rejected by this stable-release workflow. Update both files
before creating a version tag. `RELEASE_TAG=vX.Y.Z just dist-build` validates the
version and passes the explicit tag to cargo-dist.

Release archives and the runtime image are built and the image is smoke-tested
before publication. The same locally tested image is pushed to GHCR without a
second build. GHCR publication and GitHub Release creation are separate external
operations; a network failure during publication may still require a rerun.

The workflows use the repository's default `GITHUB_TOKEN`; publishing jobs grant
it package and release write permissions, so no additional registry secret is
required for GHCR.

## Checks

```sh
just fmt
just check                     # fmt-check, Clippy, Rust and release-validation tests
just test-integration
just docker-smoke macdack:dev-amd64
```

The smoke test requires Docker or Podman and curl. It starts a temporary CSV HTTP
source in `nginx:mainline-alpine`, verifies health, metrics, and CSV persistence,
then restarts the server while the source is unavailable. UDP/67 is not published
to the host. Created containers and the network are removed; temporary files
remain at the printed path for diagnostics.

Testing the complete client -> relay -> server path and BIOS/UEFI/iPXE requires
an isolated Linux network and test bootloaders. Creating network namespaces
requires `CAP_NET_ADMIN` or root; never point these tests at a production DHCP
network. Building for another architecture is not a substitute for running and
testing on that architecture.

## License

`macdack` is licensed under the MIT License. See [LICENSE](LICENSE). Third-party
dependencies remain under their respective licenses and are not relicensed by
this project.
