# Iroh dag sync

Example how to use iroh protocols such as gossip and iroh-bytes, as well as
iroh components such as the blob store, to sync possibly very deep DAGs like you
would have when working with IPFS data, unixfs directories etc.

As an added complexity, we will support non-BLAKE3 hash functions.

# Getting started

- First, generate some data.

We need a car file. You can just import some directory into ipfs and then export
it as a car file. Make sure to use --raw-leaves to have a more interesting dag.

Let's use the linux kernel sources as an example.

```
> ipfs add ../linux --raw-leaves
> ipfs dag export QmWyLtd4WEJe45UBqCZG94gYY9B8qF3k4DKFX3o2bodHmV > linux.car
```

- Import the data:

```
> cargo run --release import linux.car
...
root: QmWyLtd4WEJe45UBqCZG94gYY9B8qF3k4DKFX3o2bodHmV
```

This will create two databases in the current directory. dag.db contains
information about the structure of the dag, blobs.db (a directory) contains
the raw data.

- Start a node that makes the data available

```
> cargo run --release node
I am irgkesdtbih664hq2fjgd6zf7g6mazqkr7deqzplavmwl3vdbboa
```

- Now try to sync

In a *different directory*, start the sync process:

```
> mkdir tmp
> cd tmp
> cargo run --release sync --from irgkesdtbih664hq2fjgd6zf7g6mazqkr7deqzplavmwl3vdbboa QmWyLtd4WEJe45UBqCZG94gYY9B8qF3k4DKFX3o2bodHmV
```

This will traverse the entire DAG in depth-first, pre-order, left-to-right
traversal order. Which may take a while. But - it is just a single request/
response pair, so we will saturate the wire.

- Export the synced data

```
> cargo run --release export QmWyLtd4WEJe45UBqCZG94gYY9B8qF3k4DKFX3o2bodHmV --target output.car
```

Export without specifying a target just dumps the cids to stdout.

# Advanced use

## Specifying traversal options

When traversing DAGs, you can specify not just the root of a dag, but a more
complex traversal config in [ron] notation.

E.g. the command line below will fully traverse the DAG, but omit all cids with
a codec of Raw (0x55).

This is the "stem" of the dag, all non-leaf nodes, or to be precise all nodes
that could potentially contain links.

```
> cargo run --release export --traversal 'Full(root:"QmWyLtd4WEJe45UBqCZG94gYY9B8qF3k4DKFX3o2bodHmV",filter:NoRaw)'
```

## Specifying inline options

For each blob of a traversal, the sender runs a predicate that decides whether
the data should be inlined or not. E.g. we might want to sync small dag nodes
immediately, but leave syncing large leafs to a different protocol.

We can specify the predicate using the inline argument.

For inlined blobs, we will receive the blake3 hash and the data for each blob.
For non-inlined blobs we will just receive the blake3 hash for the blob. We can
then get the data by another means if we need it.

E.g. the example below syncs the linux kernel, but only the non-leaf blocks.
We get the mapping from cid to blake3 hash for all leaf blocks and can get them
by another means if we need them.

```
> cargo run --release export --traversal 'Full(root:"QmWyLtd4WEJe45UBqCZG94gYY9B8qF3k4DKFX3o2bodHmV")' --inline NoRaw
```

**Security note**: we must not rely on the mapping from cid to blake3
hash, but most validate the mapping once we get the data by another means.

Otherwise somebody might force us to download completely unrelated data. For
inlined data this check against the hash contained in the cid is already taken
care of.

# Implementation

## Local store

### Non-blake3 hashes

We reuse the iroh-blobs store, but have an additional table that maps
from a non-blake3 hash to a blake3 hash. This table is only populated
with local, validated data and is therefore assumed to be correct.

### IPLD formats and links

We have an additional table that contains a mapping from a blake3 hash
and an ipld codec/format to a sequence of links. Links in this case are cids.

## Sync algorithm

The sync algorithm is centered around deterministic traversals of DAGs.

### Deterministic traversals

A deterministic traversal of a complete DAG is simple. *Any* traversal is
deterministic as long as it does not intentionally introduce randomness using
e.g. random number generators or use of randomized hash based data structures.

A deterministic traversal for an incomplete DAG is simply a traversal that
stops as soon as some block that might contain links can not be found.

### Sync algorithm details

To sync between two nodes, alice (sender) and bob (receiver), bob configures a
deterministic traversal, for example a root cid and a traversal order. He
communicates this information to alice.

Alice now executes the traversal and loads the data for each cid, then
sends it back to bob as a bao4 encoded message. The message starts with the
blake3 hash, the size as a le encoded u64, and then the bao encoded chunks of
the data. We use a chunk group size of 4, so chunk groups are 2^4*1024 = 16 KiB.

Bob executes *the same* deterministic traversal while receiving data from alice.
Every item from alice corresponds to a cid in the deterministic traversal.

As an item is received, bob validates the item by computing the non-blake3 hash,
then adds the data to the iroh blob store, and extracts links from the blob
according to the format contained in the cid.

Only the reception of this additional data might allow the traversal on bob's
side to continue. So it is important that the traversal is as lazy as possible
in traversing blobs.

### Possible traversals

The above approach will work for any traversal provided that it is
deterministic and that the same traversal is executed on both sides.

- A traversal that has a continuous path from the root to each DAG node is
guaranteed to complete even if bob has incomplete data or no data at all, since
the DAG is built from the root. E.g. a traversal of a DAG that *omits* leaf
nodes.

- A traversal that does _not_ have a continuous path from the root to each DAG
node relies on data already being present. E.g. a traversal that only produces
leaf nodes. It will stop if some of the data needed for the traversal is not
already present on the receiver node.

- Traversals can be composed. E.g. you could have a traversal that only produces
leafs of a dag, then a second stage that lets though all cids where the hash
ends with an odd number, or filters based on a bitmap.

- The simplest possible traversal is to just return the root cid. Using this,
this protocol can be used to retrieve individual blobs or sequences of unrelated
blobs.

### Possible strategy to sync deep DAGs with lots of data.

Assuming you are connected to several nodes that each have a chain-like DAG
with some big data blobs hanging off each chain node. A possible strategy
to quickly sync could be the following:

- Sync the stem of the chain from multiple or even all neighbouring nodes.
- Once this is done, or slightly staggered, sync the leafs of the chain
    from multiple nodes in such a way that the download work is divided.
    E.g. odd hashes from node A, even hashes from node B.

Alternatively the second step could be done as multiple single-cid sync requests
to neighbours in a round robin way.

First step: just get the branch nodes
```
cargo run --release sync --from bsmlrj4sodhaivs2r7tssw4zeasqqr42lk6xt4e42ikzazkp4huq --traversal 'Full(root:"QmWyLtd4WEJe45UBqCZG94gYY9B8qF3k4DKFX3o2bodHmV",filter:NoRaw)'
```

Second step: get the leaf nodes. Note that this query requires the first query for the
traversal on the receiver side to be even possible.
```
cargo run --release sync --from bsmlrj4sodhaivs2r7tssw4zeasqqr42lk6xt4e42ikzazkp4huq --traversal 'Full(root:"QmWyLtd4WEJe45UBqCZG94gYY9B8qF3k4DKFX3o2bodHmV",filter:JustRaw)'
```

## Network protocol

A request consists of traversal options and options to configure the inline
predicate. In this example we are using a simple postcard-encoded rust enum
for this, and using [ron] for parsing the enum.

What traversals there should be is probably highly project dependent, so
typically you would just define a custom ALPN and then define a number of
possible traversals that are appropriate for your project.

[ron]: https://docs.rs/ron/0.8.1/ron/#rusty-object-notation
