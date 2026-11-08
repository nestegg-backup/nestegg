# Desktop stand-in: Ubuntu LTS with the official static proxmox-backup-client.
ARG BASE=ubuntu:24.04
FROM ${BASE}
ARG KEYRING_SHA256=136673be77aba35dcce385b28737689ad64fd785a797e57897589aed08db6e45
RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends ca-certificates curl \
 && curl -fsSL -o /usr/share/keyrings/proxmox-archive-keyring.gpg \
      https://enterprise.proxmox.com/debian/proxmox-archive-keyring-trixie.gpg \
 && echo "${KEYRING_SHA256}  /usr/share/keyrings/proxmox-archive-keyring.gpg" | sha256sum -c - \
 && printf 'Types: deb\nURIs: http://download.proxmox.com/debian/pbs-client\nSuites: trixie\nComponents: main\nSigned-By: /usr/share/keyrings/proxmox-archive-keyring.gpg\n' \
      > /etc/apt/sources.list.d/pbs-client.sources \
 && apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends proxmox-backup-client-static \
 && rm -rf /var/lib/apt/lists/*
