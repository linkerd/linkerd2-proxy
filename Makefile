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
CARGO_TEST = $(CARGO) test --frozen $(RELEASE) $(TEST_FLAGS)

$(TARGET_BIN): fetch
	$(CARGO_BUILD)

$(PKG_ROOT)/$(PKG): $(TARGET_BIN)
	mkdir -p $(PKG_BASE)/bin
	cp LICENSE $(PKG_BASE)
	cp $(TARGET_BIN) $(PKG_BASE)/bin
	cd $(PKG_ROOT) && \
		tar -czvf $(PKG) $(PKG_NAME)
	rm -rf $(PKG_BASE)
	cd $(PKG_ROOT) && \
		($(SHASUM) $(PKG) >$(PACKAGE_VERSION).txt) && \
		cp $(PACKAGE_VERSION).txt latest.txt

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

.PHONY: clean-package
clean-package:
	rm -rf $(PKG_ROOT)

.PHONY: all
all: build test
