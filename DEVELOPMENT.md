# Development Guidelines

## Devcontainer Setup

Devcontainer setup is easy. Ensure your host have OpenZFS installed, then create test zpools from host. Then you can
see them in the container and use it.

Prevent create pools from the container because it requires device access to host, which is hard from container. Label
test zpool so other people can see it:

```
root@localhost:/var/zfs-pools# zpool create -f vp1 ./vp1
root@localhost:/var/zfs-pools# zpool create -f vp2 ./vp2
root@localhost:/var/zfs-pools# zpool create -f vp3 ./vp3
root@localhost:/var/zfs-pools# zpool set user:isdev=yes vp1
root@localhost:/var/zfs-pools# zpool set user:isdev=yes vp2
root@localhost:/var/zfs-pools# zpool set user:isdev=yes vp3
```