CARGO_TARGET ?= $(shell rustup show |sed -n 's/^Default host: \(.*\)/\1/p')
TARGET = target/$(CARGO_TARGET)/debug
ifdef CARGO_RELEASE
	RELEASE = --release
	TARGET = target/$(CARGO_TARGET)/release
endif

ifndef PACKAGE_VERSION
	PACKAGE_VERSION = $(shell git rev-parse --short HEAD)
endif

ARCH ?= amd64
STRIP ?= strip
PROFILING = profiling
TARGET_BIN = $(TARGET)/linkerd2-proxy
PKG_ROOT = $(TARGET)/package
PKG_NAME = linkerd2-proxy-$(PACKAGE_VERSION)-$(ARCH)
PKG_BASE = $(PKG_ROOT)/$(PKG_NAME)
PKG_CHECKSEC = $(PKG_BASE)-checksec.json
PKG = $(PKG_NAME).tar.gz

SHASUM = shasum -a 256

CARGO ?= cargo
CARGO_BUILD = $(CARGO) build --frozen $(RELEASE) --target $(CARGO_TARGET)
CARGO_TEST = $(CARGO) test --frozen $(RELEASE) --target $(CARGO_TARGET)
CARGO_FMT = $(CARGO) fmt --all
CARGO_CLIPPY = $(CARGO) clippy

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

SHELLCHECK ?= shellcheck
SHELLCHECK_CMD = $(SHELLCHECK) -x -P "$(CURDIR)/profiling"

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
		($(SHASUM) $(PKG) >$(PKG_NAME).txt)
	rm -rf $(PKG_BASE)


.PHONY: fetch
fetch: Cargo.lock
	$(CARGO) fetch --locked

.PHONY: check
check: fetch
	$(CARGO) check

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

.PHONY: lint
lint:
	$(CARGO_CLIPPY) --all-targets


.PHONY: shellcheck
shellcheck:
	$(SHELLCHECK_CMD) $$(find "$(CURDIR)" -type f \
		! -path "$(CURDIR)"/.git/hooks/\*.sample \
		| while read -r f; do [ "$$(file -b --mime-type "$$f")" = 'text/x-shellscript' ] && printf '%s\0' "$$f"; done | xargs -0)

.PHONY: test
test: fetch
	$(CARGO_TEST)

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

