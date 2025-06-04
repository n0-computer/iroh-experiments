# Iroh content discovery

This rust workspace provides global content discovery for iroh.

*iroh-content-discovery* is a library that provides a discovery protocol,
a client implementation, and a client command line utility.

*iroh-content-tracker* is a server implementation for the content discovery
protocol.

## Building from source

Make sure you have an up to date version of [rust](https://www.rust-lang.org/) installed. Use the
[rustup](https://rustup.rs/) tool to get the rust compiler `rustc` and build tool
`cargo` for your platform.

Then run `cargo build --release` from the root directory. The resulting binary
will be in `target/release/iroh-content-tracker`

## Running the tracker

```sh
iroh-content-tracker
```

Will run the server with a persistent node id and announce information.

## Announcing content

When announcing content, you can give either iroh tickets or content hashes.

You will need to provide an ANNOUNCE_SECRET that is the secret key of the node id
that provides the data.

```sh
iroh-content-discovery-cli announce \
    --tracker b223f67b76e1853c7f76d9a9f8ce4d8dbb04a48ad9631ce52347043388475767 \
    blobabgu47eqpaveknv56nty4mfpzlan6i4y3lctwxvwpqbun5uxdh5eiaaaagtcpbvnzwwhxvhk765vpy6ulrdnslajjuicbpxodxwhubcjjvyua
```

## Querying content

When querying content, you can use tickets, hashes, or hash and format.

When using tickets, the address part of the ticket will be ignored.

```sh
iroh-content-discovery-cli query \
    --tracker b223f67b76e1853c7f76d9a9f8ce4d8dbb04a48ad9631ce52347043388475767 \
    blobabgu47eqpaveknv56nty4mfpzlan6i4y3lctwxvwpqbun5uxdh5eiaaaagtcpbvnzwwhxvhk765vpy6ulrdnslajjuicbpxodxwhubcjjvyua
```

## Verification

Verification works in different ways depending if the content is partial or
complete.

For partial content, the tracker will just ask for the unverified content size.
That's the only thing you can do for a node that possibly has just started
downloading the content itself.

For full content and blobs, the tracker will choose a random blake3 chunk of the
data and download it. This is relatively cheap in terms of traffic (2 KiB), and
since the chunk is random a host that has only partial content will be found
eventually.

For full content and hash sequences such as collections, the tracker will choose
a random chunk of a random child.
