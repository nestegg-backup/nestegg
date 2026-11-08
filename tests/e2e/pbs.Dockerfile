# Disposable Proxmox Backup Server for end-to-end tests (official packages).
FROM debian:trixie
ARG KEYRING_SHA256=136673be77aba35dcce385b28737689ad64fd785a797e57897589aed08db6e45
RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends ca-certificates curl \
 && curl -fsSL -o /usr/share/keyrings/proxmox-archive-keyring.gpg \
      https://enterprise.proxmox.com/debian/proxmox-archive-keyring-trixie.gpg \
 && echo "${KEYRING_SHA256}  /usr/share/keyrings/proxmox-archive-keyring.gpg" | sha256sum -c - \
 && printf 'Types: deb\nURIs: http://download.proxmox.com/debian/pbs\nSuites: trixie\nComponents: pbs-no-subscription\nSigned-By: /usr/share/keyrings/proxmox-archive-keyring.gpg\n' \
      > /etc/apt/sources.list.d/pbs.sources \
 && apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends proxmox-backup-server proxmox-backup-client \
 && rm -rf /var/lib/apt/lists/*
COPY pbs-entrypoint.sh /usr/local/bin/pbs-entrypoint.sh
EXPOSE 8007
ENTRYPOINT ["/usr/local/bin/pbs-entrypoint.sh"]
