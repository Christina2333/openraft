use std::fmt::Display;
use std::fmt::Formatter;

use serde::Deserialize;
use serde::Serialize;

use crate::LogId;
use crate::SnapshotSegmentId;

impl From<(u64, u64)> for LogId {
    fn from(v: (u64, u64)) -> Self {
        LogId::new(v.0, v.1)
    }
}

impl Display for LogId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}-{}", self.term, self.index)
    }
}

impl LogId {
    pub fn new(term: u64, index: u64) -> Self {
        if term == 0 || index == 0 {
            assert_eq!(index, 0, "zero-th log entry must be (0,0), but {}, {}", term, index);
            assert_eq!(term, 0, "zero-th log entry must be (0,0), but {}, {}", term, index);
        }
        LogId { term, index }
    }
}

pub trait LogIdOptionExt {
    fn index(&self) -> Option<u64>;
    fn next_index(&self) -> u64;
}

impl LogIdOptionExt for Option<LogId> {
    fn index(&self) -> Option<u64> {
        self.map(|x| x.index)
    }

    fn next_index(&self) -> u64 {
        match self {
            None => 0,
            Some(log_id) => log_id.index + 1,
        }
    }
}

pub trait LogIndexOptionExt {
    fn next_index(&self) -> u64;
    fn prev_index(&self) -> Self;
    fn add(&self, v: u64) -> Self;
}

impl LogIndexOptionExt for Option<u64> {
    fn next_index(&self) -> u64 {
        match self {
            None => 0,
            Some(v) => v + 1,
        }
    }

    fn prev_index(&self) -> Self {
        match self {
            None => {
                panic!("None has no previous value");
            }
            Some(v) => {
                if *v == 0 {
                    None
                } else {
                    Some(*v - 1)
                }
            }
        }
    }

    fn add(&self, v: u64) -> Self {
        Some(self.next_index() + v).prev_index()
    }
}

impl<D: ToString> From<(D, u64)> for SnapshotSegmentId {
    fn from(v: (D, u64)) -> Self {
        SnapshotSegmentId {
            id: v.0.to_string(),
            offset: v.1,
        }
    }
}

impl Display for SnapshotSegmentId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}+{}", self.id, self.offset)
    }
}

// An update action with option to update with some value or just leave it as is.
#[derive(Debug, Clone, PartialOrd, PartialEq, Eq, Serialize, Deserialize)]
pub enum Update<T> {
    Update(T),
    AsIs,
}
