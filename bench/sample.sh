#!/bin/bash
# Every second: unix time, then RSS (KiB) and cumulative CPU seconds of the
# TurboCI service and of a gitlab-runner running as the given systemd unit.
# Usage: sample.sh GITLAB_RUNNER_UNIT > samples.txt
T=$(systemctl show -p MainPID --value turboci)
G=$(systemctl show -p MainPID --value "${1:?gitlab-runner unit}")
tick=$(getconf CLK_TCK)
while true; do
  echo "$(date +%s)" \
    "$(awk '/VmRSS/{print $2}' /proc/$T/status)" "$(awk -v t=$tick '{print ($14+$15)/t}' /proc/$T/stat)" \
    "$(awk '/VmRSS/{print $2}' /proc/$G/status)" "$(awk -v t=$tick '{print ($14+$15)/t}' /proc/$G/stat)"
  sleep 1
done
