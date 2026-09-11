APP_ID := simple-pdf-viewer
PREFIX ?= $(HOME)/.local
BINDIR := $(PREFIX)/bin
DATADIR := $(PREFIX)/share
ICONDIR := $(DATADIR)/icons/hicolor/512x512/apps
APPLICATIONDIR := $(DATADIR)/applications

.PHONY: install

install:
	cargo install --path . --locked --root "$(PREFIX)"
	install -Dm644 logo.png "$(ICONDIR)/$(APP_ID).png"
	install -Dm644 assets/$(APP_ID).desktop "$(APPLICATIONDIR)/$(APP_ID).desktop"
	@echo "Installed $(APP_ID) to $(BINDIR) and desktop assets to $(DATADIR)."
