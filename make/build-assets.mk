# One session per top-level invocation; recursive make inherits its references.
ifndef ASSET_SESSION
ASSET_SESSION := $(shell python3 -c 'import uuid; print(uuid.uuid4().hex)')
endif
ASSET_REF_DIR ?= $(abspath .cache/build-assets/$(ASSET_SESSION))
DBX_OUTPUT_REF ?= $(ASSET_REF_DIR)/dbx.ref
export ASSET_SESSION ASSET_REF_DIR DBX_OUTPUT_REF
