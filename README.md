# Iroh Experiments

This is for experiments with iroh by the n0 team. Things in here can be very
low level and unpolished.

Some of the things in this repo might make it to [iroh-examples] or even into
iroh itself, most will not.

## [Content-discovery](content-discovery)

A complete content disccovery system for iroh, including a tracker, a client
crate to use the tracker, and [pkarr] integration for finding trackers.

## [h3-iroh](h3-iroh)

Sending HTTP3 requests over iroh connections via the [h3](https://crates.io/crates/h3) crate.

## [Iroh-dag-sync](iroh-dag-sync)

An experiment trying to bridge the gap between iroh-blobs and IPFS, allowing you to sync
IPFS DAGs with non-BLAKE3 CIDs via iroh-blobs.

## [Iroh-pkarr-naming-system](iroh-pkarr-naming-system)

Experiment of how to do something similar to [ipns] using [pkarr] and the
bittorrent [mainline] DHT.

## [Iroh-s3-bao-store](iroh-s3-bao-store)

An iroh-blobs store implementation that keeps the data on s3. Useful to provide
content-addressing to existing public resources.

[iroh-examples]: https://github.com/n0-computer/iroh-examples
[pkarr]: https://pkarr.org/
[ipns]: https://docs.ipfs.tech/concepts/ipns/
[mainline]: https://en.wikipedia.org/wiki/Mainline_DHT
