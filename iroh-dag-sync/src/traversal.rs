use std::{collections::HashSet, pin::Pin};

use futures_lite::Future;

use crate::{
    protocol::{
        Cid, FullTraversalOpts, InlineOpts, SequenceTraversalOpts, TraversalFilter, TraversalOpts,
    },
    tables::ReadableTables,
};

/// A DAG traversal over a database.
///
/// This is very similar to a stream of Cids, but gives mutable access to the
/// database between calls to `next` to perform changes.
pub trait Traversal {
    type Db;

    fn roots(&self) -> Vec<Cid>;

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

impl<D> Traversal for BoxedTraversal<'_, D> {
    type Db = D;

    fn roots(&self) -> Vec<Cid> {
        self.0.roots()
    }

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
    fn roots(&self) -> Vec<Cid>;
}

impl<D, T: Traversal<Db = D>> BoxableTraversal<D> for BoxableTraversalImpl<D, T> {
    fn next(&mut self) -> Pin<Box<dyn Future<Output = anyhow::Result<Option<Cid>>> + '_>> {
        Box::pin(self.inner.next())
    }

    fn db_mut(&mut self) -> &mut D {
        self.inner.db_mut()
    }

    fn roots(&self) -> Vec<Cid> {
        self.inner.roots()
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

    fn roots(&self) -> Vec<Cid> {
        self.inner.roots()
    }
}

pub struct SequenceTraversal<T> {
    cids: std::vec::IntoIter<Cid>,
    db: T,
}

impl<D> SequenceTraversal<D> {
    pub fn new(db: D, cids: Vec<Cid>) -> Self {
        Self {
            cids: cids.into_iter(),
            db,
        }
    }
}

impl<D> Traversal for SequenceTraversal<D> {
    type Db = D;

    async fn next(&mut self) -> anyhow::Result<Option<Cid>> {
        Ok(self.cids.next())
    }

    fn db_mut(&mut self) -> &mut D {
        &mut self.db
    }

    fn roots(&self) -> Vec<Cid> {
        self.cids.clone().collect()
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
        let stack = vec![root];
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

    fn roots(&self) -> Vec<Cid> {
        self.stack.clone()
    }

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
                        self.stack.push(link.into());
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

pub fn get_traversal<'a, D: ReadableTables + Unpin + 'a>(
    opts: TraversalOpts,
    db: D,
) -> anyhow::Result<BoxedTraversal<'a, D>> {
    Ok(match opts {
        TraversalOpts::Sequence(SequenceTraversalOpts(cids)) => {
            SequenceTraversal::new(db, cids).boxed()
        }
        TraversalOpts::Full(FullTraversalOpts {
            root,
            visited,
            filter,
            ..
        }) => {
            let visited = visited.unwrap_or_default();
            let filter = filter.unwrap_or_default();
            let traversal = FullTraversal::new(db, root, visited.into_iter().collect());
            match filter {
                TraversalFilter::All => traversal.boxed(),
                TraversalFilter::NoRaw => traversal.filter(|cid| cid.codec() != 0x55).boxed(),
                TraversalFilter::JustRaw => traversal.filter(|cid| cid.codec() == 0x55).boxed(),
                TraversalFilter::Excude(codecs) => {
                    let codecs: HashSet<u64> = codecs.into_iter().collect();
                    traversal
                        .filter(move |cid| !codecs.contains(&cid.codec()))
                        .boxed()
                }
            }
        }
    })
}

pub type InlineCb = Box<dyn Fn(&Cid) -> bool>;

pub fn get_inline(inline: InlineOpts) -> anyhow::Result<InlineCb> {
    Ok(match inline {
        InlineOpts::All => Box::new(|_| true),
        InlineOpts::NoRaw => Box::new(|cid| cid.codec() != 0x55),
        InlineOpts::Excude(codecs) => {
            let codecs: HashSet<u64> = codecs.into_iter().collect();
            Box::new(move |cid| !codecs.contains(&cid.codec()))
        }
        InlineOpts::None => Box::new(|_| false),
    })
}
