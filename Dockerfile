# syntax=docker/dockerfile:1.7
#
# Runtime-only container for the gateway. The binary is built outside
# the Dockerfile (CI builds it in the `build` job, local devs run
# `mise run build` first) and dropped in via COPY. The CSS bundle and
# datastar.js are baked into the binary at compile time via
# `include_bytes!`, so this image needs no asset directory.
#
# Minimal apt-get on the critical path: `reqwest` is built with the
# `rustls-tls` feature which links `webpki-roots` — the Mozilla CA
# bundle is compiled into the binary, so outbound HTTPS from the
# *Rust* side (upstream LLMs, OIDC) doesn't read `/etc/ssl/certs`. And
# the gateway is a pure tokio process for chat / proxy / OIDC — no
# `fork()` / no shell-outs — so tokio's own signal handling reaps
# SIGTERM cleanly without `tini`. We omit `tini`.
#
# We DO need two extra packages for the RAG indexer:
#
#   * `git` — the worker shells out to `git clone --depth 1` +
#     `git fetch` (`server::rag::git`) to materialise operator-
#     configured collections into the named volume. `gix` would avoid
#     this dep but at ~100 transitive crates, which trades a 30 MB
#     layer for a noisy build tree.
#
#   * `ca-certificates` — git is a separate process and validates
#     TLS via the OS trust store, NOT `webpki-roots`. Without this
#     package `git clone https://github.com/...` fails with
#     `Problem with the SSL CA cert (path? access rights?)`. The Rust
#     binary still gets its CAs from `webpki-roots`, so this package
#     is RAG-only — but it's installed unconditionally because the
#     image is one artifact.
#
# Both are installed with `--no-install-recommends` so the layer
# stays as small as Debian allows (drops `git-man`, `liberror-perl`,
# etc.). This image build depends on deb.debian.org being up for
# this one step — keep an eye on it during outages.
#
# Local build (after `mise run build`):
#   docker build -t gateway:dev .
#
# CI build: the `container` job in .github/workflows/ci.yml builds this
# file with docker/build-push-action, with `target/release/gateway`
# arriving from the `ci` job's artifact.

ARG DEBIAN_CODENAME=trixie

FROM debian:${DEBIAN_CODENAME}-slim AS runtime

RUN apt-get update \
 && apt-get install -y --no-install-recommends git ca-certificates \
 && rm -rf /var/lib/apt/lists/*

RUN groupadd --system --gid 1000 gateway \
 && useradd  --system --uid 1000 --gid gateway --home /app gateway

WORKDIR /app
COPY --chown=gateway:gateway target/release/gateway /app/gateway
# `typst` CLI for the `typst_<template>` tools. Downloaded by the
# `fetch-typst-cli` mise task (which runs as a dep of `mise run
# build` — see mise.toml) so it arrives in the same artifacts/
# context that the CI/local build reads from. Stays out of apt
# entirely so the runtime image keeps its deb.debian.org-free
# property (see the file header).
COPY --chown=root:root --chmod=0755 target/release/typst /usr/local/bin/typst

# Sample typst templates (the example `letter`: template.typ + manifest +
# logo.svg + bundled Urbanist fonts). Baked into the image so the
# `typst_<template>` tools ship WITH the binary that renders them — one
# immutable artifact, versioned by the pipeline. Point the gateway's
# `[typst] templates_dir` at this path. To ship your own templates instead,
# replace this directory or point `templates_dir` at a separate mounted
# directory — a host mount placed over THIS path would shadow the baked-in
# templates and silently pin an old design. The compile only reads from
# here, so the read-only image layer is enough — no writable volume needed.
COPY --chown=gateway:gateway examples/typst-templates /opt/typst-templates

# pdfium shared library for the `fetch_attachment` PDF reader's image
# tier (scanned PDFs → page images for a vision model). Staged into
# target/release/ by the `fetch-pdfium` mise task, a dep of `mise run
# build` — the same artifact pipeline as the gateway + typst binaries,
# so it's always present in a CI or `mise run build` context (this COPY
# fails the build if it isn't — run `mise run fetch-pdfium` first). The
# gateway loads it at runtime from `PDFIUM_LIB_PATH` (set below); the
# text-extraction tier needs no native lib, so a PDF's text still reads
# even if pdfium ever fails to load. Chromium's pdfium is BSD-3-Clause.
# `0644` is enough — it's dlopen'd, not exec'd.
COPY --chown=root:root --chmod=0644 target/release/libpdfium.so /usr/local/lib/libpdfium.so

USER gateway

# Rama listens on the address resolved from the IP/PORT env vars; binding
# 0.0.0.0 inside a container is what makes the published port reachable.
# PDFIUM_LIB_PATH points the PDF reader at the bundled pdfium above.
ENV IP=0.0.0.0 \
    PORT=8080 \
    RUST_LOG=info,gateway=info \
    PDFIUM_LIB_PATH=/usr/local/lib/libpdfium.so

EXPOSE 8080

CMD ["/app/gateway"]
