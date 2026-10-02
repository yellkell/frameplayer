# FramePlayer developer entry points. Everything here wraps cargo or tools/.
#   make frame-go      push a dev build to the paired headset, launch, tail logs
#   make build         release build for the Steam Frame (aarch64)
#   make check         fmt + clippy + tests (what CI runs)

CARGO       ?= cargo
TARGET      ?= aarch64-unknown-linux-gnu
IMAGE       ?= frameplayer-build:latest
QEMU_IMAGE  ?= frameplayer-qemu-test:latest
VERSION     ?= $(shell sed -n '/^\[workspace.package\]/,/^\[/{s/^version *= *"\(.*\)"/\1/p}' Cargo.toml | head -n1)

.PHONY: help build build-host test fmt lint check installer \
        frame-pair frame-push frame-go frame-launch frame-logs frame-shell frame-status frame-perf \
        docker-image docker-build qemu-image qemu-test release test-videos test-videos-quick clean-dist

help:
	@sed -n 's/^#   //p' $(MAKEFILE_LIST)
	@echo "targets: build build-host test lint check installer frame-{pair,push,go,launch,logs,shell,status,perf}"
	@echo "         docker-image docker-build qemu-image qemu-test release test-videos"

build:
	$(CARGO) build --release --target $(TARGET) -p fp-app

build-host:
	$(CARGO) build --workspace

test:
	$(CARGO) test --workspace

fmt:
	$(CARGO) fmt --all

lint:
	$(CARGO) fmt --all -- --check
	$(CARGO) clippy --workspace --all-targets -- -D warnings

check: lint test

# Desktop installer for the current machine.
installer:
	$(CARGO) build --release -p fp-installer
	@echo "built target/release/frameplayer-install"

frame-pair:
	tools/frame.sh pair

frame-push:
	tools/frame.sh push

frame-go:
	tools/frame.sh go

frame-launch:
	tools/frame.sh launch

frame-logs:
	tools/frame.sh logs -f

frame-shell:
	tools/frame.sh shell

frame-status:
	tools/frame.sh status

frame-perf:
	tools/perf-capture.sh --launch --duration 120

# aarch64 build image with the SLR 4 sysroot and static media libraries.
docker-image:
	docker build --platform linux/arm64 -f docker/Dockerfile.aarch64 -t $(IMAGE) docker

docker-build:
	docker run --rm --platform linux/arm64 -v "$(CURDIR):/src" -w /src \
	  -v frameplayer-cargo:/usr/local/cargo/registry $(IMAGE) \
	  cargo build --release --target $(TARGET) -p fp-app

qemu-image:
	docker build -f docker/Dockerfile.qemu-test -t $(QEMU_IMAGE) docker

# Run the workspace's unit tests as aarch64 binaries under qemu-user.
qemu-test:
	docker run --rm -v "$(CURDIR):/src" -w /src -v frameplayer-cargo-qemu:/usr/local/cargo/registry \
	  $(QEMU_IMAGE) cargo test --workspace --target $(TARGET)

release:
	tools/release.sh --version $(VERSION)

test-videos:
	tools/gen-test-videos.sh --out test-videos

test-videos-quick:
	tools/gen-test-videos.sh --quick --eye 512 --duration 3 --out test-videos

clean-dist:
	rm -rf dist/out
