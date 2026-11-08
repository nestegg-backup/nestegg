# Desktop stand-in with systemd as PID 1, for package and service tests.
ARG CLIENT_IMAGE=nestegg-e2e-client:ubuntu-24.04
FROM ${CLIENT_IMAGE}
RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends systemd systemd-sysv dbus sudo \
 && rm -rf /var/lib/apt/lists/* \
 && systemctl mask getty.target systemd-logind.service console-getty.service
STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
