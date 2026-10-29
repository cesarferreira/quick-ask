APP_NAME := Quick Ask.app
INSTALL_DIR ?= /Applications

.PHONY: install clean

install:
	./scripts/package.sh
	-pkill -x quick-ask
	rm -rf "$(INSTALL_DIR)/$(APP_NAME)"
	cp -R "dist/$(APP_NAME)" "$(INSTALL_DIR)/"
	open "$(INSTALL_DIR)/$(APP_NAME)"

clean:
	cargo clean
	rm -rf dist
