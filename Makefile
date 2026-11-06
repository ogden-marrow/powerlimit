# Plain-make wrapper around cargo.  See CMakeLists.txt for the CMake route.
PREFIX  ?= /usr/local
BINDIR  ?= $(PREFIX)/bin
DATADIR ?= $(PREFIX)/share
POLKIT  ?= /usr/share/polkit-1/actions

all: build

build:
	cargo build --release

# no build dependency: sudo has no cargo in PATH, so run `make` first as your user
install:
	@test -x target/release/powerlimit || { echo "run 'make' first (as your normal user)"; exit 1; }
	install -Dm755 target/release/powerlimit $(DESTDIR)$(BINDIR)/powerlimit
	install -Dm644 data/io.github.powerlimit.desktop $(DESTDIR)$(DATADIR)/applications/io.github.powerlimit.desktop
	install -Dm644 data/powerlimit.svg $(DESTDIR)$(DATADIR)/icons/hicolor/scalable/apps/powerlimit.svg
	for n in 16 32 48 64 128 256 512; do install -Dm644 data/icons/hicolor/$${n}x$${n}/apps/powerlimit.png $(DESTDIR)$(DATADIR)/icons/hicolor/$${n}x$${n}/apps/powerlimit.png; done
	sed 's|@CMAKE_INSTALL_FULL_BINDIR@|$(BINDIR)|' data/org.powerlimit.policy.in > data/org.powerlimit.policy
	install -Dm644 data/org.powerlimit.policy $(DESTDIR)$(POLKIT)/org.powerlimit.policy
	rm -f data/org.powerlimit.policy

uninstall:
	rm -f $(DESTDIR)$(BINDIR)/powerlimit \
	      $(DESTDIR)$(DATADIR)/applications/io.github.powerlimit.desktop $(DESTDIR)$(DATADIR)/applications/powerlimit.desktop \
	      $(DESTDIR)$(DATADIR)/icons/hicolor/*/apps/powerlimit.png \
	      $(DESTDIR)$(DATADIR)/icons/hicolor/scalable/apps/powerlimit.svg \
	      $(DESTDIR)$(POLKIT)/org.powerlimit.policy

clean:
	cargo clean

VERSION := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
ARCH    := $(shell dpkg --print-architecture 2>/dev/null || echo amd64)
DEB     := dist/powerlimit_$(VERSION)_$(ARCH).deb

# Debian/Ubuntu package: `make deb` -> dist/powerlimit_<version>_<arch>.deb
deb: build
	rm -rf build/deb && mkdir -p build/deb/DEBIAN dist
	$(MAKE) install DESTDIR=$(CURDIR)/build/deb PREFIX=/usr POLKIT=/usr/share/polkit-1/actions
	sed -e 's/@VERSION@/$(VERSION)/' -e 's/@ARCH@/$(ARCH)/' packaging/control.in > build/deb/DEBIAN/control
	install -m755 packaging/postinst build/deb/DEBIAN/postinst
	fakeroot dpkg-deb --build build/deb $(DEB)
	@echo "built $(DEB)"

.PHONY: all build install uninstall clean deb
