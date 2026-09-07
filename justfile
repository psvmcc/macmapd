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

dist-plan:
    dist plan

dist-build: build-amd64

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

test-release:
    python3 -B -m unittest discover -s tests -p 'test_release_*.py'

check: fmt-check lint test test-release

docker-build-amd64:
    docker buildx build --platform linux/amd64 --load --tag '{{image}}:{{tag}}-amd64' .

# Only this recipe publishes images. Supply the full registry/name and tag.
docker-push image_name version:
    docker buildx build --platform linux/amd64 --tag "$1:$2" --push .

docker-smoke image_name:
    CONTAINER_ENGINE=docker sh scripts/docker-smoke.sh "$1"

container-build:
    {{engine}} build --platform linux/amd64 --tag '{{image}}:{{tag}}-amd64' .

container-smoke image_name:
    CONTAINER_ENGINE={{engine}} sh scripts/docker-smoke.sh "$1"

podman-build-amd64:
    podman build --platform linux/amd64 --tag '{{image}}:{{tag}}-amd64' .
