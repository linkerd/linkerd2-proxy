GIT_REV = $(shell git rev-parse --short HEAD)

RELEASE =
TARGET = target/debug
VERBOSE =

ifdef CARGO_RELEASE
	RELEASE = --release
	TARGET = target/release
endif

TARGET_BIN = $(TARGET)/linkerd2-proxy
PKG_ROOT = $(TARGET)/package
PKG_NAME = linkerd2-proxy-$(GIT_REV)
PKG_BASE = $(PKG_ROOT)/$(PKG_NAME)
PKG = $(PKG_NAME).tar.gz

CARGO = cargo
ifdef CARGO_VERBOSE
	CARGO = cargo --verbose
endif

CARGO_BUILD = $(CARGO) build --frozen $(RELEASE)

TEST_FLAGS =
ifdef TEST_FLAKEY
	TEST_FLAGS = --no-default-features
endif
CARGO_TEST = $(CARGO) test --frozen $(RELEASE) $(TEST_FLAGS)

.PHONY: fetch
fetch: Cargo.lock
	$(CARGO) fetch --locked

.PHONY: build
build: $(TARGET_BIN)

.PHONY: test
test: fetch
	$(CARGO_TEST)

.PHONY: test-benches
test-benches: fetch
	$(CARGO_TEST) --benches

.PHONY: package
package: $(PKG_ROOT)/$(PKG)

.PHONY: all
all: build test

$(TARGET_BIN): fetch
	$(CARGO_BUILD)

$(PKG_ROOT)/$(PKG): $(TARGET_BIN)
	mkdir -p $(PKG_BASE)/bin
	cp LICENSE $(PKG_BASE)
	cp $(TARGET_BIN) $(PKG_BASE)/bin
	cd $(PKG_ROOT) && tar -czvf $(PKG) $(PKG_NAME)
	rm -rf $(PKG_BASE)
