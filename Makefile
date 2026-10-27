.PHONY: build-in-docker

OUT_DIR ?= dist
PLATFORM ?= linux/$(shell uname -m | sed -e 's/^x86_64$$/amd64/' -e 's/^aarch64$$/arm64/')

build-in-docker:
	docker buildx build --platform "$(PLATFORM)" \
		--file dockerfiles/Dockerfile --target artifacts \
		--build-arg SOURCE_COMMIT="$$(git rev-parse HEAD)" \
		--output "type=local,dest=$(OUT_DIR)" .
