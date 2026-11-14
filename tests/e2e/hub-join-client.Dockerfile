# A fresh desktop: neither nestegg nor the Proxmox client/repository is present.
FROM ubuntu:24.04
RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      systemd systemd-sysv dbus sudo curl ca-certificates udev e2fsprogs \
      iproute2 python3 openssl util-linux \
 && rm -rf /var/lib/apt/lists/* \
 && systemctl mask getty.target systemd-logind.service console-getty.service \
 && useradd -m alice \
 && printf 'alice ALL=(ALL) NOPASSWD: ALL\n' > /etc/sudoers.d/alice \
 && chmod 440 /etc/sudoers.d/alice \
 && printf 'household backup sentinel\n' > /home/alice/notes.txt \
 && chown alice:alice /home/alice/notes.txt
STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
