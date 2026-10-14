# Uses docker if installed, otherwise podman (same compose file works with both).
CLI := $(shell command -v docker >/dev/null 2>&1 && echo docker || echo podman)
DATABASE_URL ?= postgres://dodo:dodo@localhost:5432/dodo

.PHONY: up down logs e2e test

up:    ## build and start app + db + mock PSP
	$(CLI) compose up --build -d

down:  ## stop everything and wipe the database
	$(CLI) compose down -v

logs:  ## follow the mock PSP logs (webhooks arrive here)
	$(CLI) compose logs -f psp

e2e: up  ## end-to-end checks against the running stack (~1 min)
	@for i in $$(seq 60); do curl -s -o /dev/null localhost:8080 && break; sleep 1; done
	scripts/e2e.sh

test:  ## cargo tests (needs only the db container)
	$(CLI) compose up -d db
	DATABASE_URL=$(DATABASE_URL) cargo test
