# Build locally, deploy to the host named in secrets/deploy.env (gitignored).
-include secrets/deploy.env

BIN   := target/release/hermes-beaver
STAGE := /tmp/hermes-beaver-deploy
SSH   := ssh -o BatchMode=yes

.PHONY: build test lint demo deploy check-env

build:
	cargo build --release

test:
	cargo test

lint:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings

demo:
	cargo run --release -- demo

check-env:
	@test -n "$(DEPLOY_HOST)" -a -n "$(DEPLOY_USER)" || { echo "Missing secrets/deploy.env: cp -r secrets.example secrets"; exit 1; }

deploy: check-env build
	$(SSH) $(DEPLOY_HOST) "rm -rf $(STAGE) && mkdir -m 700 $(STAGE)"
	scp -q $(BIN) deploy/install.sh deploy/hermes-beaver.service deploy/hermes-beaver-autostart.desktop $(DEPLOY_HOST):$(STAGE)/
	@if [ -f secrets/config.toml ]; then scp -q secrets/config.toml $(DEPLOY_HOST):$(STAGE)/config.toml; fi
	$(SSH) $(DEPLOY_HOST) "DEPLOY_USER=$(DEPLOY_USER) bash $(STAGE)/install.sh $(STAGE) && rm -rf $(STAGE)"
