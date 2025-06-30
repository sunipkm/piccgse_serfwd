#!/usr/bin/env bash

while :; do
    case $1 in
        --force) FORCE=1 ;;
        --help) echo "Usage: $0 [--force]"; exit 0 ;;
        *) break ;;
    esac
    shift
done
if [ -n "$FORCE" ]; then
    echo "Force mode enabled. All files will be pushed."
else
    echo "Normal mode. Only new or modified files will be pushed."
fi

TARGET="armv7-unknown-linux-gnueabihf"
REMOTE_TARGET=$(ssh piccgsepi uname -m)
if [ $? -eq 0 ]; then
    if [ $REMOTE_TARGET = "aarch64" ]; then
        echo "Remote system is ARM64."
        TARGET="aarch64-unknown-linux-gnu"
    fi
fi
cross build --target $TARGET --release
ssh piccgsepi -t 'mkdir -p ~/serial-server'
ssh -q piccgsepi -t 'stat "serial-server/serial-server" &> /dev/null'
if [ $? -ne 0 ] || [ -n "$FORCE" ]; then
    scp target/$TARGET/release/piccgse_serfwd piccgsepi:~/serial-server
else
    echo "Not updating server binary."
fi
ssh -q piccgsepi -t 'stat "serial-server/serial.env" &> /dev/null'
if [ $? -ne 0 ]; then
    scp serial.env gse-serial.service piccgsepi:~/serial-server
fi
