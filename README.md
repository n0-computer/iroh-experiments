# Iroh Experiments

[![Chat](https://img.shields.io/discord/1161119546170687619?logo=discord&style=flat-square)](https://discord.com/invite/DpmJgtU7cW)
[![Youtube](https://img.shields.io/badge/YouTube-red?logo=youtube&logoColor=white&style=flat-square)](https://www.youtube.com/@n0computer)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg?style=flat-square)](LICENSE-MIT)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg?style=flat-square)](LICENSE-APACHE)
[![CI](https://img.shields.io/github/actions/workflow/status/n0-computer/iroh-experiments/ci.yml?branch=main&style=flat-square&label=CI)](https://github.com/n0-computer/iroh-experiments/actions/workflows/ci.yml)

This is for experiments with [iroh] by the n0 team. Things in here can be very
low level and unpolished.

Some of the things in this repo might make it to [iroh-examples] or even into
[iroh] itself, most will not.

## [content-discovery](content-discovery)

A complete content disccovery system for iroh, including a tracker, a client
crate to use the tracker, and [pkarr] integration for finding trackers.

## [h3-iroh](h3-iroh)

Sending HTTP3 requests over iroh connections via the [h3](https://crates.io/crates/h3) crate.

## [iroh-dag-sync](iroh-dag-sync)

An experiment trying to bridge the gap between iroh-blobs and IPFS, allowing you to sync
IPFS DAGs with non-BLAKE3 CIDs via iroh-blobs.

## [iroh-pkarr-naming-system](iroh-pkarr-naming-system)

Experiment of how to do something similar to [ipns] using [pkarr] and the
bittorrent [mainline] DHT.

## [iroh-s3-bao-store](iroh-s3-bao-store)

An iroh-blobs store implementation that keeps the data on s3. Useful to provide
content-addressing to existing public resources.

# License

Copyright 2025 N0, INC.

This project is licensed under either of

 * Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or
   http://www.apache.org/licenses/LICENSE-2.0)
 * MIT license ([LICENSE-MIT](LICENSE-MIT) or
   http://opensource.org/licenses/MIT)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this project by you, as defined in the Apache-2.0 license,
shall be dual licensed as above, without any additional terms or conditions.

[iroh]: https://github.com/n0-computer/iroh
[iroh-examples]: https://github.com/n0-computer/iroh-examples
[pkarr]: https://pkarr.org/
[ipns]: https://docs.ipfs.tech/concepts/ipns/
[mainline]: https://en.wikipedia.org/wiki/Mainline_DHT
