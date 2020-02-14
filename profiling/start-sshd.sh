#!/bin/bash
set -o errexit
set -o pipefail

mkdir -p /root/.ssh/
cat /root/authorized_keys > /root/.ssh/authorized_keys
service sshd restart
