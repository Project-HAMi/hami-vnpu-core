# Official CANN 8.5.2 SDK image, pinned by its multi-architecture digest.
ARG SDK_IMAGE=quay.io/ascend/cann:8.5.2-910b-ubuntu22.04-py3.11@sha256:093e4003c81ef9795a0c1019f6df46e0823ef38ba15ab68c5ed2b236b4a1d64b
FROM ${SDK_IMAGE} AS build

ARG SDK_IMAGE
ARG TARGETARCH
ARG RUST_VERSION=1.94.0
ARG SOURCE_COMMIT=unknown
ENV ASCEND_TOOLKIT_HOME=/usr/local/Ascend/cann-8.5.2 \
    ASCEND_DRIVER_HOME=/usr/local/Ascend/driver \
    PATH=/root/.cargo/bin:${PATH}
# Put genuine SDK/driver libraries before the base image's devlib directory.
ENV LD_LIBRARY_PATH=${ASCEND_TOOLKIT_HOME}/lib64:${ASCEND_DRIVER_HOME}/lib64/driver:${ASCEND_DRIVER_HOME}/lib64/common:${LD_LIBRARY_PATH}

RUN case "${TARGETARCH}/$(uname -m)" in \
      amd64/x86_64|arm64/aarch64) ;; \
      *) echo "Unsupported build architecture: ${TARGETARCH}/$(uname -m)" >&2; exit 1 ;; \
    esac \
    && apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl gcc libc6-dev python3 binutils \
    && rm -rf /var/lib/apt/lists/*

# Extract only real HDK user libraries for API verification. No kernel driver is installed.
# Official download reference: Ascend/docs, _static/package_info.js.
RUN case "${TARGETARCH}" in \
      amd64) driver_arch=x86-64; driver_sha256=f64012342cdf72d44f527efdef1304af41aedc73b202bf0d91243d21765555f7 ;; \
      arm64) driver_arch=aarch64; driver_sha256=1573563e63cb893cf3049ae31259eb8d6c31ec859be82c3c4a9c5846eea372b5 ;; \
      *) echo "Unsupported HDK architecture: ${TARGETARCH}" >&2; exit 1 ;; \
    esac \
    && curl --proto '=https' --tlsv1.2 -fSL \
      "https://ascend-repo.obs.cn-east-2.myhuaweicloud.com/Ascend%20HDK/Ascend%20HDK%2025.5.1/Ascend-hdk-910b-npu-driver_25.5.1_linux-${driver_arch}.run" \
      -o /tmp/driver.run \
    && echo "${driver_sha256}  /tmp/driver.run" | sha256sum -c - \
    && bash /tmp/driver.run --noexec --nox11 --extract=/tmp/hdk \
    && mkdir -p "${ASCEND_DRIVER_HOME}/lib64/driver" "${ASCEND_DRIVER_HOME}/lib64/common" \
    && cp -a /tmp/hdk/driver/lib64/driver/. "${ASCEND_DRIVER_HOME}/lib64/driver/" \
    && cp -a /tmp/hdk/driver/lib64/common/. "${ASCEND_DRIVER_HOME}/lib64/common/" \
    && cp /tmp/hdk/driver/lib64/libdcmi.so "${ASCEND_DRIVER_HOME}/lib64/driver/libdcmi.so" \
    && cp /tmp/hdk/version.info "${ASCEND_DRIVER_HOME}/version.info" \
    && rm -rf /tmp/driver.run /tmp/hdk
RUN curl --proto '=https' --tlsv1.2 -fsS https://sh.rustup.rs -o /tmp/rustup-init.sh \
    && sh /tmp/rustup-init.sh -y --no-modify-path --profile minimal --default-toolchain "${RUST_VERSION}" \
    && rm /tmp/rustup-init.sh

WORKDIR /build
COPY . .
RUN cargo build --release --workspace --locked \
    && mkdir /out \
    && cp target/release/libvnpu.so target/release/vnpu-dcmi /out/ \
    && python3 scripts/ci/verify-release.py /out/libvnpu.so \
         --arch "$(uname -m)" --source-commit "${SOURCE_COMMIT}" --sdk-image "${SDK_IMAGE}" \
         --cann-version 8.5.2 --require-spatial-apis \
         --output /out/build-info.json \
    && cd /out && sha256sum libvnpu.so vnpu-dcmi build-info.json > SHA256SUMS

# Export the paired release artifacts and their provenance; no SDK or test link stubs.
FROM scratch AS artifacts
COPY --from=build /out/ /
