set positional-arguments

image := env_var_or_default("IMAGE", "macmapd")
tag := env_var_or_default("TAG", "dev")
artifacts := env_var_or_default("ARTIFACTS", "dist")
engine := env_var_or_default("CONTAINER_ENGINE", "podman")

default:
    @just --list

build:
    cargo build --locked

build-release:
    cargo build --locked --release

build-amd64:
    CONTAINER_ENGINE={{engine}} sh scripts/dist-build-container.sh x86_64-unknown-linux-gnu

build-arm64:
    CONTAINER_ENGINE={{engine}} sh scripts/dist-build-container.sh aarch64-unknown-linux-gnu

dist-plan:
    dist plan

dist-build: build-amd64 build-arm64

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

lint:
    cargo clippy --locked --all-targets -- -D warnings

test:
    cargo test --locked --lib --bins

test-integration:
    cargo test --locked --test integration --test protocol

check: fmt-check lint test

docker-build-amd64:
    docker buildx build --platform linux/amd64 --load --tag '{{image}}:{{tag}}-amd64' .

docker-build-arm64:
    docker buildx build --platform linux/arm64 --load --tag '{{image}}:{{tag}}-arm64' .

docker-build-multi:
    mkdir -p '{{artifacts}}'
    docker buildx build --platform linux/amd64,linux/arm64 --tag '{{image}}:{{tag}}' --output 'type=oci,dest={{artifacts}}/macmapd.oci.tar' .

# Only this recipe publishes images. Supply the full registry/name and tag.
docker-push image_name version:
    docker buildx build --platform linux/amd64,linux/arm64 --tag "$1:$2" --push .

docker-smoke image_name:
    CONTAINER_ENGINE=docker sh scripts/docker-smoke.sh "$1"

container-build architecture="arm64":
    {{engine}} build --platform 'linux/{{architecture}}' --tag '{{image}}:{{tag}}-{{architecture}}' .

container-smoke image_name:
    CONTAINER_ENGINE={{engine}} sh scripts/docker-smoke.sh "$1"

podman-build-amd64:
    podman build --platform linux/amd64 --tag '{{image}}:{{tag}}-amd64' .

podman-build-arm64:
    podman build --platform linux/arm64 --tag '{{image}}:{{tag}}-arm64' .

podman-cross-amd64:
    podman build --platform linux/amd64 --file Dockerfile.cross --tag '{{image}}:{{tag}}-amd64' .
