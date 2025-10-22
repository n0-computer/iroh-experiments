use std::{
    collections::BTreeMap,
    fmt::Debug,
    sync::{Arc, Mutex},
};

use tokio::task::JoinHandle;
use tokio_util::task::AbortOnDropHandle;

/// A map of long lived or infinite tasks.
#[derive(Clone, Debug)]
pub struct TaskMap<T: Ord + Debug>(Arc<Inner<T>>);

impl<T: Ord + Debug> TaskMap<T> {
    /// Create a new task map.
    pub fn publish(&self, key: T, task: JoinHandle<()>) {
        let mut tasks = self.0.tasks.lock().unwrap();
        tasks.insert(key, AbortOnDropHandle::new(task));
    }

    pub fn retain(&self, f: impl Fn(&T) -> bool) {
        let mut tasks = self.0.tasks.lock().unwrap();
        tasks.retain(|k, _| f(k));
    }
}

impl<T: Ord + Debug> Default for TaskMap<T> {
    fn default() -> Self {
        Self(Default::default())
    }
}

#[derive(Debug)]
struct Inner<T: Ord> {
    tasks: Mutex<BTreeMap<T, AbortOnDropHandle<()>>>,
}

impl<T: Ord + Debug> Default for Inner<T> {
    fn default() -> Self {
        Self {
            tasks: Default::default(),
        }
    }
}
