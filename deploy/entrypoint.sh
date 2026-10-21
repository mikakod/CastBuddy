#!/bin/sh
# First run: drop a container-appropriate default config onto the /data
# volume (0.0.0.0 binds inside the container, /media as the library, the
# generated MediaMTX config on the writable volume). Edit it afterwards -
# /data/castbuddy.toml survives image upgrades.
set -eu
if [ ! -f /data/castbuddy.toml ]; then
    cp /app/castbuddy.docker.toml /data/castbuddy.toml
    echo "castbuddy: wrote default config to /data/castbuddy.toml"
fi
exec castbuddy /data/castbuddy.toml
