#!/bin/sh
set -eu

# The Docker volumes are initially owned by root, while lifecycle commands run as vscode.
sudo chown "$(id -u):$(id -g)" \
  /home/vscode/app/target \
  /usr/local/cargo/registry \
  /usr/local/cargo/git

cd /home/vscode/app
cargo fetch --locked
