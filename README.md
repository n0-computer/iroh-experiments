# Iroh Experiments

This is for experiments with iroh by the n0 team. Things in here can be very
low level and unpolished.

Some of the things in this repo might make it to [iroh-examples] or even into
iroh itself, most will not.

## Iroh-dag-sync

An experiment how we could deal with DAGs in iroh, as well as how to safely
handle non-blake3 hash functions.

## Content-discovery

A complete content disccovery system for iroh, including a tracker, a client
crate to use the tracker, and [pkarr] integration for finding trackers.

## Iroh-pkarr-naming-system

Experiment how to do something similar to [ipns] using [pkarr] and the
bittorrent [mainline] DHT.

## Iroh-s3-bao-store

An iroh-blobs store implementation that keeps the data on s3. Useful to provide
content-addressing to existing public resources.

[iroh-examples]: https://github.com/n0-computer/iroh-examples
[pkarr]: https://pkarr.org/
[ipns]: https://docs.ipfs.tech/concepts/ipns/
[mainline]: https://en.wikipedia.org/wiki/Mainline_DHT
