CARGO_TARGET ?= $(shell rustup show |sed -n 's/^Default host: \(.*\)/\1/p')
TARGET = target/$(CARGO_TARGET)/debug
ifdef CARGO_RELEASE
	RELEASE = --release
	TARGET = target/$(CARGO_TARGET)/release
endif

ifndef PACKAGE_VERSION
	PACKAGE_VERSION = $(shell git rev-parse --short HEAD)
endif

STRIP ?= strip
PROFILING = profiling
TARGET_BIN = $(TARGET)/linkerd2-proxy
PKG_ROOT = $(TARGET)/package
PKG_NAME = linkerd2-proxy-$(PACKAGE_VERSION)
PKG_BASE = $(PKG_ROOT)/$(PKG_NAME)
PKG_CHECKSEC = $(PKG_BASE)-checksec.json
PKG = $(PKG_NAME).tar.gz

SHASUM = shasum -a 256

CARGO ?= cargo
CARGO_BUILD = $(CARGO) build --frozen $(RELEASE) --target $(CARGO_TARGET)
CARGO_TEST = $(CARGO) test --frozen $(RELEASE) --target $(CARGO_TARGET)
CARGO_FMT = $(CARGO) fmt --all

DOCKER = docker
DOCKER_BUILD = docker build
ifdef DOCKER_TAG
	DOCKER_BUILD = docker build -t $(DOCKER_TAG)
endif
ifdef DOCKER_UNOPTIMIZED
	DOCKER_BUILD += --build-arg="PROXY_UNOPTIMIZED=$(DOCKER_UNOPTIMIZED)"
endif

RUSTCFLAGS ?=
ifdef CARGO_DEBUG
	RUSTCFLAGS += -C debuginfo=2
endif

$(TARGET_BIN): fetch
	$(CARGO_BUILD) -p linkerd2-proxy

$(PKG_ROOT)/$(PKG) $(PKG_CHECKSEC): $(TARGET_BIN)
	mkdir -p $(PKG_BASE)/bin
	cp LICENSE $(PKG_BASE)
	cp $(TARGET_BIN) $(PKG_BASE)/bin/linkerd2-proxy
	$(STRIP) $(PKG_BASE)/bin/linkerd2-proxy
ifdef CARGO_DEBUG
	if which objcopy >/dev/null ; then \
		objcopy $(TARGET_BIN) $(PKG_BASE)/linkerd2-proxy.obj ; \
		chmod 644 $(PKG_BASE)/linkerd2-proxy.obj ; \
	fi
endif
	./checksec.sh $(PKG_BASE)/bin/linkerd2-proxy >$(PKG_CHECKSEC) || true
	cd $(PKG_ROOT) && \
		tar -czvf $(PKG) $(PKG_NAME) && \
		($(SHASUM) $(PKG) >$(PKG_NAME).txt) && \
		rm -rf $(PKG_BASE)


.PHONY: fetch
fetch: Cargo.lock
	$(CARGO) fetch --locked

.PHONY: build
build: $(TARGET_BIN)

.PHONY: clean
clean:
	$(CARGO) clean --target-dir $(TARGET)

.PHONY: check-fmt
check-fmt:
	$(CARGO_FMT) -- --check

.PHONY: fmt
fmt:
	$(CARGO_FMT)


.PHONY: test-lib
test-lib:: fetch
	$(CARGO_TEST) --lib

.PHONY: test-integration
test-integration: fetch
	$(CARGO_TEST) --tests

.PHONY: test
test: test-lib test-integration

.PHONY: test-flakey
test-flakey: fetch
	$(CARGO_TEST) --features linkerd-app-integration/flaky_tests

.PHONY: package
package: $(PKG_ROOT)/$(PKG)

.PHONY: clean-package
clean-package:
	rm -rf $(PKG_ROOT)

.PHONY: clean-profile
clean-profile:
	rm -rf target/release/profile*
	rm -rf target/profile/*

.PHONY: docker
docker: Dockerfile Cargo.lock
	DOCKER_BUILDKIT=1 $(DOCKER_BUILD) .

.PHONY: all
all: build test

