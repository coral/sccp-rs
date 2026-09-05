# syntax=docker/dockerfile:1

FROM rust:bookworm AS asterisk-source

SHELL ["/bin/bash", "-o", "pipefail", "-c"]

RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        binutils bison build-essential ca-certificates clang flex git libclang-dev \
        libedit-dev libjansson-dev libsqlite3-dev libssl-dev libxml2-dev pkg-config \
        uuid-dev default-libmysqlclient-dev default-mysql-client default-mysql-server \
        libpq-dev postgresql postgresql-client sqlite3 \
    && rm -rf /var/lib/apt/lists/*

ARG ASTERISK_REF=22.7.0

RUN git init -q /opt/asterisk-source \
    && git -C /opt/asterisk-source remote add origin https://github.com/asterisk/asterisk.git \
    && git -C /opt/asterisk-source fetch --depth 1 origin "${ASTERISK_REF}" \
    && git -C /opt/asterisk-source checkout --detach FETCH_HEAD \
    && cd /opt/asterisk-source \
    && ./configure \
        --prefix=/opt/asterisk-live \
        --sysconfdir=/opt/asterisk-live/etc \
        --localstatedir=/opt/asterisk-live/var \
        --without-pjproject-bundled \
        --without-jansson-bundled \
    && make menuselect.makeopts \
    && make include/asterisk/buildopts.h

ENV ASTERISK_SOURCE_DIR=/opt/asterisk-source
ENV ASTERISK_BUILD_DIR=/opt/asterisk-source
ENV RUSTFLAGS=-Dwarnings

FROM asterisk-source AS asterisk-live

RUN cd /opt/asterisk-source \
    && menuselect/menuselect \
        --disable BUILD_NATIVE \
        --enable res_config_sqlite3 \
        --enable res_config_pgsql \
        --enable res_config_mysql \
        menuselect.makeopts \
    && make -j2 \
    && make install \
    && test -f /opt/asterisk-live/lib/asterisk/modules/res_config_sqlite3.so \
    && test -f /opt/asterisk-live/lib/asterisk/modules/res_config_pgsql.so \
    && test -f /opt/asterisk-live/lib/asterisk/modules/res_config_mysql.so

FROM asterisk-source AS artifact-build

ARG ASTERISK_FEATURE
ARG ARTIFACT_VARIANT=normal
ARG MODULE_VERSION
ARG TARGETARCH

WORKDIR /workspace
COPY . .

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/workspace/target \
    cargo build --locked --release -p asterisk-module --no-default-features \
        --features "${ASTERISK_FEATURE}" \
    && case "${TARGETARCH}" in \
        amd64) artifact_arch=x86_64; elf_machine='Advanced Micro Devices X86-64' ;; \
        arm64) artifact_arch=aarch64; elf_machine=AArch64 ;; \
        *) printf 'unsupported Docker target architecture: %s\n' "${TARGETARCH}" >&2; exit 2 ;; \
    esac \
    && readelf -h target/release/libchan_sccp2.so > /tmp/chan-sccp2-elf-header \
    && grep -q "Machine:.*${elf_machine}" /tmp/chan-sccp2-elf-header \
    && readelf -d target/release/libchan_sccp2.so > /tmp/chan-sccp2-elf-dynamic \
    && grep -q 'SONAME.*chan_sccp2.so' /tmp/chan-sccp2-elf-dynamic \
    && nm -D --defined-only target/release/libchan_sccp2.so > /tmp/chan-sccp2-exported-symbols \
    && grep -q ' T __internal_chan_sccp2_self$' /tmp/chan-sccp2-exported-symbols \
    && ! grep -Eq ' [TDB] (rust_sccp_|sccp_ast_)' /tmp/chan-sccp2-exported-symbols \
    && mkdir /output \
    && [[ "${MODULE_VERSION}" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$ ]] \
    && case "${ARTIFACT_VARIANT}" in \
        normal) \
            artifact_prefix=chan_sccp2-asterisk; \
            ! strings target/release/libchan_sccp2.so | grep -F sccp.dbg.coral.works >/dev/null \
            ;; \
        debug) \
            artifact_prefix=chan_sccp2-asterisk-debug; \
            strings target/release/libchan_sccp2.so | grep -F sccp.dbg.coral.works >/dev/null \
            ;; \
        *) printf 'unsupported artifact variant: %s\n' "${ARTIFACT_VARIANT}" >&2; exit 2 ;; \
    esac \
    && cp target/release/libchan_sccp2.so \
        "/output/${artifact_prefix}-linux-${artifact_arch}-${MODULE_VERSION}.so"

FROM scratch AS artifact
COPY --from=artifact-build /output/ /

FROM asterisk-live AS bridge-test

ARG ASTERISK_FEATURE
WORKDIR /workspace
COPY . .

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/workspace/target \
    cargo build --locked --release -p asterisk-module --no-default-features \
        --features "${ASTERISK_FEATURE},live-asterisk-tests" \
    && ASTERISK_BIN=/opt/asterisk-live/sbin/asterisk \
        ASTERISK_MODULE_DIR=/opt/asterisk-live/lib/asterisk/modules \
        ASTERISK_DATA_DIR=/opt/asterisk-live/var/lib/asterisk \
        ./asterisk-module/ci/live-tests/test-bridges.sh target/release/libchan_sccp2.so

FROM asterisk-live AS realtime-test

WORKDIR /workspace
COPY . .

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/workspace/target \
    cargo build --locked --release -p asterisk-module --no-default-features --features asterisk-22 \
    && ASTERISK_BIN=/opt/asterisk-live/sbin/asterisk \
        ASTERISK_MODULE_DIR=/opt/asterisk-live/lib/asterisk/modules \
        ASTERISK_DATA_DIR=/opt/asterisk-live/var/lib/asterisk \
        ./asterisk-module/ci/realtime/test-live-provider.sh sqlite target/release/libchan_sccp2.so \
    && service postgresql start \
    && su postgres -c 'createuser root' \
    && su postgres -c 'createdb --owner=root sccp2_realtime' \
    && ASTERISK_BIN=/opt/asterisk-live/sbin/asterisk \
        ASTERISK_MODULE_DIR=/opt/asterisk-live/lib/asterisk/modules \
        ASTERISK_DATA_DIR=/opt/asterisk-live/var/lib/asterisk \
        ./asterisk-module/ci/realtime/test-live-provider.sh postgresql target/release/libchan_sccp2.so \
    && service postgresql stop \
    && service mariadb start \
    && mysql --user=root --execute 'CREATE DATABASE sccp2_realtime CHARACTER SET utf8mb4' \
    && ASTERISK_BIN=/opt/asterisk-live/sbin/asterisk \
        ASTERISK_MODULE_DIR=/opt/asterisk-live/lib/asterisk/modules \
        ASTERISK_DATA_DIR=/opt/asterisk-live/var/lib/asterisk \
        ./asterisk-module/ci/realtime/test-live-provider.sh mysql target/release/libchan_sccp2.so

FROM asterisk-live AS sorcery-ari-test

RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends curl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /workspace
COPY . .

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/workspace/target \
    cargo build --locked --release -p asterisk-module --no-default-features --features asterisk-22 \
    && ASTERISK_BIN=/opt/asterisk-live/sbin/asterisk \
        ASTERISK_MODULE_DIR=/opt/asterisk-live/lib/asterisk/modules \
        ASTERISK_DATA_DIR=/opt/asterisk-live/var/lib/asterisk \
        ./asterisk-module/ci/sorcery/test-live-ari.sh target/release/libchan_sccp2.so
