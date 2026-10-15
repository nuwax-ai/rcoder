# Curated Kani formal gate. Keep this separate from make test.
# The harness allowlist and per-proof timeout live in tools/verify_kani.py.

.PHONY: verify-kani

verify-kani:
	@python3 -m unittest tools.test_verify_kani
	@python3 tools/verify_kani.py
