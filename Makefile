TARGET = target/debug
ifdef CARGO_RELEASE
	RELEASE = --release
	TARGET = target/release
endif

ifndef PACKAGE_VERSION
	PACKAGE_VERSION = $(shell git rev-parse --short HEAD)
endif

TARGET_BIN = $(TARGET)/linkerd2-proxy
PKG_ROOT = $(TARGET)/package
PKG_NAME = linkerd2-proxy-$(PACKAGE_VERSION)
PKG_BASE = $(PKG_ROOT)/$(PKG_NAME)
PKG = $(PKG_NAME).tar.gz

SHASUM = shasum -a 256

CARGO ?= cargo
CARGO_BUILD = $(CARGO) build --frozen $(RELEASE)
CARGO_TEST = $(CARGO) test --all --frozen $(RELEASE)
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
	$(CARGO_BUILD)

$(PKG_ROOT)/$(PKG): $(TARGET_BIN)
	mkdir -p $(PKG_BASE)/bin
	cp LICENSE $(PKG_BASE)
	cp $(TARGET_BIN) $(PKG_BASE)/bin/linkerd2-proxy
	strip $(PKG_BASE)/bin/linkerd2-proxy
ifdef CARGO_DEBUG
	if which objcopy >/dev/null ; then \
		objcopy $(TARGET_BIN) $(PKG_BASE)/linkerd2-proxy.obj ; \
		chmod 644 $(PKG_BASE)/linkerd2-proxy.obj ; \
	fi
endif
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
	$(CARGO_TEST) --lib --no-default-features

.PHONY: test-integration
test-integration: fetch
	$(CARGO_TEST) --tests --no-default-features

.PHONY: test
test: test-lib test-integration

.PHONY: test-flakey
test-flakey: fetch
	$(CARGO_TEST)

.PHONY: package
package: $(PKG_ROOT)/$(PKG)

.PHONY: clean-package
clean-package:
	rm -rf $(PKG_ROOT)

.PHONY: docker
docker: Dockerfile Cargo.lock
	$(DOCKER_BUILD) .

.PHONY: all
all: build test
