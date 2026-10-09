#!/bin/sh

# Install ZFS in Debian 13 trixie

set -e

# Fork as root to ensure we have the necessary permissions
if [ "$(id -u)" -ne 0 ]; then
  echo "This script must be run as root. Exiting."
  exit 1
fi

# Ensure zfs exists in /proc/modules
if ! grep -q '^zfs ' /proc/modules; then
  echo "ZFS module is not loaded. Exiting."
  exit 1
fi

# Get version of the loaded ZFS module
ZFS_VERSION=$(cut -d '-' -f 1 < /sys/module/zfs/version)
if [ -z "$ZFS_VERSION" ]; then
  echo "Failed to get ZFS module version. Exiting."
  exit 1
fi
echo "Loaded ZFS module version: $ZFS_VERSION"

cat <<EOF >/etc/apt/sources.list.d/trixie-backports.list
deb http://deb.debian.org/debian trixie-backports main contrib non-free-firmware
deb-src http://deb.debian.org/debian trixie-backports main contrib non-free-firmware
EOF

cat <<EOF >/etc/apt/preferences.d/90_zfs
Package: *
Pin: release a=trixie-backports
Pin-Priority: 500
EOF


apt update

ZFS_PACKAGE_VERSION=$(apt-cache policy "zfsutils-linux" | grep "$ZFS_VERSION" | grep Candidate | awk '{print $2}')
if [ -z "$ZFS_PACKAGE_VERSION" ]; then
  echo "Failed to get ZFS package version for ZFS module $ZFS_VERSION. Exiting."
  exit 1
fi
echo "ZFS package version to install: $ZFS_PACKAGE_VERSION"

#apt install -y linux-headers-$(dpkg --print-architecture)
#apt install -y zfs-dkms
apt install -y "zfsutils-linux=${ZFS_PACKAGE_VERSION}"