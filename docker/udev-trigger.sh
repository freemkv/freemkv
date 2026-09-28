#!/bin/sh
# Triggered by udev on disc insertion. Notifies autorip web server.
# Uses busybox `sh` + `wget` so the FROM scratch image doesn't need
# bash or curl (v0.25.7 image diet).
[ -f /etc/autorip.env ] && . /etc/autorip.env
PORT="${PORT:-8080}"
# --method (not --post-data) sent NOTHING since v0.25.7: this image's busybox
# 1.36 wget has no --method option and dies on the unrecognized flag before
# connecting, silently (backgrounded, stdout+stderr to /dev/null). --post-data
# forces POST on both GNU and busybox wget. -T bounds a wedged/overloaded
# server, which would otherwise leave this backgrounded call running forever.
wget -q -O- -T 10 --post-data= "http://localhost:${PORT}/api/rip/$1" >/dev/null 2>&1 &
