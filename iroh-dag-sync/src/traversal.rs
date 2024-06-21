use std::{collections::{BTreeSet, HashSet}, pin::Pin};

use futures_lite::Future;
use libipld::Cid;
use serde::{Deserialize, Serialize};

use crate::{protocol::SCid, tables::ReadableTables};

/// A DAG traversal over a database.
///
/// This is very similar to a stream of Cids, but gives mutable access to the
/// database between calls to `next` to perform changes.
pub trait Traversal {
    type Db;

    fn next(&mut self) -> impl Future<Output = anyhow::Result<Option<Cid>>>;

    fn db_mut(&mut self) -> &mut Self::Db;

    fn filter<F: Fn(&Cid) -> bool>(self, f: F) -> Filtered<Self, F>
    where
        Self: Sized,
    {
        Filtered {
            inner: self,
            filter: f,
        }
    }

    fn boxed<'a>(self) -> BoxedTraversal<'a, Self::Db>
    where
        Self: Sized + Unpin + 'a,
    {
        BoxedTraversal(Box::pin(BoxableTraversalImpl { inner: self }))
    }
}

pub struct BoxedTraversal<'a, D>(Pin<Box<dyn BoxableTraversal<D> + Unpin + 'a>>);

impl<'a, D> Traversal for BoxedTraversal<'a, D> {
    type Db = D;

    async fn next(&mut self) -> anyhow::Result<Option<Cid>> {
        self.0.next().await
    }

    fn db_mut(&mut self) -> &mut D {
        self.0.db_mut()
    }
}

struct BoxableTraversalImpl<D, T: Traversal<Db = D>> {
    inner: T,
}

trait BoxableTraversal<D> {
    fn next(&mut self) -> Pin<Box<dyn Future<Output = anyhow::Result<Option<Cid>>> + '_>>;
    fn db_mut(&mut self) -> &mut D;
}

impl<D, T: Traversal<Db = D>> BoxableTraversal<D> for BoxableTraversalImpl<D, T> {
    fn next(&mut self) -> Pin<Box<dyn Future<Output = anyhow::Result<Option<Cid>>> + '_>> {
        Box::pin(self.inner.next())
    }

    fn db_mut(&mut self) -> &mut D {
        self.inner.db_mut()
    }
}

pub struct Filtered<T, F> {
    inner: T,
    filter: F,
}

impl<T: Traversal, F: Fn(&Cid) -> bool> Traversal for Filtered<T, F> {
    type Db = T::Db;

    async fn next(&mut self) -> anyhow::Result<Option<Cid>> {
        while let Some(item) = self.inner.next().await? {
            if (self.filter)(&item) {
                return Ok(Some(item));
            }
        }
        Ok(None)
    }

    fn db_mut(&mut self) -> &mut Self::Db {
        self.inner.db_mut()
    }
}

pub struct SingleTraversal<T> {
    cid: Option<Cid>,
    db: T,
}

impl<D> SingleTraversal<D> {
    pub fn new(db: D, cid: Cid) -> Self {
        Self { cid: Some(cid), db }
    }
}

impl<D> Traversal for SingleTraversal<D> {
    type Db = D;

    async fn next(&mut self) -> anyhow::Result<Option<Cid>> {
        Ok(self.cid.take())
    }

    fn db_mut(&mut self) -> &mut D {
        &mut self.db
    }
}

pub struct FullTraversal<D> {
    prev: Option<Cid>,
    stack: Vec<Cid>,
    visited: HashSet<Cid>,
    db: D,
}

impl<D> FullTraversal<D> {
    pub fn new(db: D, root: Cid, visited: HashSet<Cid>) -> Self {
        let mut stack = Vec::new();
        stack.push(root);
        Self {
            stack,
            visited,
            db,
            prev: None,
        }
    }
}

impl<D: ReadableTables> Traversal for FullTraversal<D> {
    type Db = D;

    async fn next(&mut self) -> anyhow::Result<Option<Cid>> {
        loop {
            // perform deferred work
            if let Some(cid) = self.prev.take() {
                if cid.codec() == 0x55 {
                    // no need to traverse raw nodes
                    continue;
                }
                if let Some(links) = self.db.links(&cid)? {
                    for link in links.into_iter().rev() {
                        self.stack.push(link);
                    }
                }
            }
            let Some(cid) = self.stack.pop() else {
                break;
            };
            if self.visited.contains(&cid) {
                continue;
            }
            self.visited.insert(cid);
            // defer the reading of the data etc, since we might not have the data yet
            self.prev = Some(cid);
            return Ok(Some(cid));
        }
        Ok(None)
    }

    fn db_mut(&mut self) -> &mut D {
        &mut self.db
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub enum TraversalOpts {
    /// A sequence of individual cids.
    ///
    /// This can be used to sync a single block or a sequence of unrelated blocks.
    /// Note that since we are getting individual blocks, the codec part of the cids
    /// is not relevant.
    Sequence(SequenceTraversalOpts),
    /// A full traversal of a DAG, with a set of already visited cids.
    Full(FullTraversalOpts),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SequenceTraversalOpts(
    /// The sequence of cids to traverse, in order.
    Vec<SCid>
);

#[derive(Debug, Serialize, Deserialize)]
pub struct FullTraversalOpts {
        /// The root of the traversal.
        /// 
        /// The codec part of the cid is relevant. E.g. for a cid with codec raw,
        /// 0x55, the traversal would always be just the root.
        root: SCid,
        /// The set of already visited cids. This can be used to abort a traversal
        /// once data that is already known is encountered.
        /// 
        /// E.g. in case of a linked list shaped dag, you would insert here
        /// the cid of the last element that you have locally.
        #[serde(default)]
        #[serde(skip_serializing_if = "is_default")]
        visited: BTreeSet<SCid>,
        /// The order in which to traverse the DAG.
        /// 
        /// Since a traversal will abort once a cid is encountered that is not
        /// present, this can influence how much data is fetched.
        #[serde(default)]
        #[serde(skip_serializing_if = "is_default")]
        order: TraversalOrder,
        /// Filter to apply to the traversal.
        #[serde(default)]
        #[serde(skip_serializing_if = "is_default")]
        filter: TraversalFilter,
}

fn is_default<T: Eq + Default>(t: &T) -> bool {
    t == &Default::default()
}

#[derive(Debug, Serialize, Deserialize, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum TraversalOrder {
    #[default]
    DepthFirstPreOrderLeftToRight
}

#[derive(Debug, Serialize, Deserialize, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum TraversalFilter {
    /// Include all cids.
    #[default]
    All,
    /// Exclude raw cids.
    NoRaw,
    /// Exclude cids with a specific codec.
    Excude(BTreeSet<u64>),
}

pub fn get_traversal<'a, D: ReadableTables + Unpin + 'a>(
    cid: Cid,
    traversal: &str,
    db: D,
) -> anyhow::Result<BoxedTraversal<'a, D>> {
    Ok(match traversal {
        "full" => FullTraversal::new(db, cid, Default::default()).boxed(),
        "full_no_raw" => FullTraversal::new(db, cid, Default::default())
            .filter(|cid| cid.codec() != 0x55)
            .boxed(),
        "single" => SingleTraversal::new(db, cid).boxed(),
        _ => anyhow::bail!("Unknown traversal method: {}", traversal),
    })
}

pub fn get_inline(inline: &str) -> anyhow::Result<Box<dyn Fn(&Cid) -> bool>> {
    Ok(match inline {
        "always" => Box::new(|_| true),
        "never" => Box::new(|_| false),
        "no_raw" => Box::new(|cid| cid.codec() != 0x55),
        _ => anyhow::bail!("Unknown inline method: {}", inline),
    })
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use crate::{protocol::SCid, traversal::{FullTraversalOpts, SequenceTraversalOpts, TraversalFilter}};

    use super::TraversalOpts;

    #[test]
    fn test_ser() {
        let tt = TraversalOpts::Sequence(SequenceTraversalOpts(vec![SCid::from_str("QmWyLtd4WEJe45UBqCZG94gYY9B8qF3k4DKFX3o2bodHmV").unwrap().into()]));
        let json = serde_json::to_string_pretty(&tt).unwrap();
        println!("{}", json);
        let tt = TraversalOpts::Full(FullTraversalOpts {
            root: SCid::from_str("QmWyLtd4WEJe45UBqCZG94gYY9B8qF3k4DKFX3o2bodHmV").unwrap().into(),
            visited: [SCid::from_str("QmWyLtd4WEJe45UBqCZG94gYY9B8qF3k4DKFX3o2bodHmV").unwrap()].into_iter().collect(),
            order: Default::default(),
            filter: TraversalFilter::NoRaw,
        });
        let json = serde_json::to_string(&tt).unwrap();
        println!("{}", json);

        let ron = ron::to_string(&tt).unwrap();
        println!("{}", ron);
    }
} 