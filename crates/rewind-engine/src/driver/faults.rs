//! What the tests do to the link between nodes: cut a node off, drop the next requests that
//! match, hold them until released.

use std::collections::BTreeSet;
use std::fmt;

use crate::ports::NodeId;
use crate::ports::link::NodeRequest;

/// What becomes of a request on the link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Traffic {
    Deliver,
    /// Lost: its asker gets `Unreachable`, a notice is gone.
    Drop,
    /// Kept until [`Driver::release`](super::Driver::release).
    Hold,
}

type Matcher = Box<dyn Fn(&NodeRequest) -> bool + Send>;

struct Rule {
    matcher: Matcher,
    traffic: Traffic,
    /// How many requests it takes before it stops; `None`: all of them.
    times: Option<usize>,
}

/// The faults of the driver's link.
#[derive(Default)]
pub struct Faults {
    cut: BTreeSet<NodeId>,
    rules: Vec<Rule>,
    /// What the faults did, for tests.
    pub dropped: usize,
    pub held: usize,
}

impl fmt::Debug for Faults {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Faults")
            .field("cut", &self.cut)
            .field("rules", &self.rules.len())
            .field("dropped", &self.dropped)
            .field("held", &self.held)
            .finish()
    }
}

impl Faults {
    /// Nothing reaches the node through the link, and nothing leaves it.
    pub fn cut(&mut self, node: NodeId) {
        self.cut.insert(node);
    }

    /// The node is reached again.
    pub fn heal(&mut self, node: &NodeId) {
        self.cut.remove(node);
    }

    /// Drops the next `times` requests that match.
    pub fn drop_next(
        &mut self,
        times: usize,
        matcher: impl Fn(&NodeRequest) -> bool + Send + 'static,
    ) {
        self.rules.push(Rule {
            matcher: Box::new(matcher),
            traffic: Traffic::Drop,
            times: Some(times),
        });
    }

    /// Holds every request that matches until released.
    pub fn hold(&mut self, matcher: impl Fn(&NodeRequest) -> bool + Send + 'static) {
        self.rules.push(Rule {
            matcher: Box::new(matcher),
            traffic: Traffic::Hold,
            times: None,
        });
    }

    /// Forgets every rule.
    pub fn clear(&mut self) {
        self.rules.clear();
    }

    pub(crate) fn cut_between(&self, a: &NodeId, b: &NodeId) -> bool {
        a != b && (self.cut.contains(a) || self.cut.contains(b))
    }

    /// What becomes of a request from `from` to `to`.
    pub(crate) fn traffic(&mut self, from: &NodeId, to: &NodeId, request: &NodeRequest) -> Traffic {
        if self.cut_between(from, to) {
            self.dropped += 1;
            return Traffic::Drop;
        }
        for rule in &mut self.rules {
            if rule.times == Some(0) || !(rule.matcher)(request) {
                continue;
            }
            if let Some(times) = &mut rule.times {
                *times -= 1;
            }
            match rule.traffic {
                Traffic::Drop => self.dropped += 1,
                Traffic::Hold => self.held += 1,
                Traffic::Deliver => {}
            }
            return rule.traffic;
        }
        Traffic::Deliver
    }
}
