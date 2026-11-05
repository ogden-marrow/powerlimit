# Plain-make wrapper around cargo.  See CMakeLists.txt for the CMake route.
PREFIX  ?= /usr/local
BINDIR  ?= $(PREFIX)/bin
DATADIR ?= $(PREFIX)/share
POLKIT  ?= /usr/share/polkit-1/actions

all: build

build:
	cargo build --release

install: build
	install -Dm755 target/release/powerlimit $(DESTDIR)$(BINDIR)/powerlimit
	install -Dm644 data/powerlimit.desktop $(DESTDIR)$(DATADIR)/applications/powerlimit.desktop
	install -Dm644 data/powerlimit.svg $(DESTDIR)$(DATADIR)/icons/hicolor/scalable/apps/powerlimit.svg
	sed 's|@CMAKE_INSTALL_FULL_BINDIR@|$(BINDIR)|' data/org.powerlimit.policy.in > data/org.powerlimit.policy
	install -Dm644 data/org.powerlimit.policy $(DESTDIR)$(POLKIT)/org.powerlimit.policy
	rm -f data/org.powerlimit.policy

uninstall:
	rm -f $(DESTDIR)$(BINDIR)/powerlimit \
	      $(DESTDIR)$(DATADIR)/applications/powerlimit.desktop \
	      $(DESTDIR)$(DATADIR)/icons/hicolor/scalable/apps/powerlimit.svg \
	      $(DESTDIR)$(POLKIT)/org.powerlimit.policy

clean:
	cargo clean

.PHONY: all build install uninstall clean
