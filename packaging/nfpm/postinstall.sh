#!/bin/sh
# Runs as root at dpkg-configure time. waylandwebstream ships a systemd *user*
# unit, which cannot be enabled at package-install time (no user session), so
# this only points the operator at the right command.
set -e

cat <<'EOF'

waylandwebstream installed.

It runs as a systemd --user service. To enable it for your user:

    systemctl --user daemon-reload
    systemctl --user enable --now waylandwebstream

Then open http://127.0.0.1:8080. It binds loopback only and runs no session app
by default; to launch a compositor/app inside the stream, customise it with:

    systemctl --user edit waylandwebstream

The server has no authentication of its own -- put an authenticating reverse
proxy in front before widening --listen-addr. See:
https://github.com/derlaft/waylandwebstream-rs

EOF

exit 0
