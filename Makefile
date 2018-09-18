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

CARGO = cargo
ifdef CARGO_VERBOSE
	CARGO = cargo --verbose
endif

CARGO_BUILD = $(CARGO) build --frozen $(RELEASE)

TEST_FLAGS =
ifndef TEST_FLAKEY
	TEST_FLAGS = --no-default-features
endif
CARGO_TEST = $(CARGO) test --all --frozen $(RELEASE) $(TEST_FLAGS)

DOCKER = docker
DOCKER_BUILD = docker build
ifdef DOCKER_TAG
	DOCKER_BUILD = docker build -t $(DOCKER_TAG)
endif

$(TARGET_BIN): fetch
	$(CARGO_BUILD)

$(PKG_ROOT)/$(PKG): $(TARGET_BIN)
	mkdir -p $(PKG_BASE)/bin
	cp LICENSE $(PKG_BASE)
	cp $(TARGET_BIN) $(PKG_BASE)/bin
	cd $(PKG_ROOT) && \
		tar -czvf $(PKG) $(PKG_NAME) && \
		($(SHASUM) $(PKG) >$(PKG_NAME).txt) && \
		cp $(PKG_NAME).txt latest.txt
	rm -rf $(PKG_BASE)

.PHONY: fetch
fetch: Cargo.lock
	$(CARGO) fetch --locked

.PHONY: build
build: $(TARGET_BIN)

.PHONY: test
test: fetch
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
