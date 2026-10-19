.PHONY: build-in-docker

OUT_DIR ?= dist

build-in-docker:
	docker buildx build --platform linux/arm64 \
		--file dockerfiles/Dockerfile --target artifacts \
		--build-arg SOURCE_COMMIT="$$(git rev-parse HEAD)" \
		--output "type=local,dest=$(OUT_DIR)" .
