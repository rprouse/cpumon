//! Process ancestry with protection against PID reuse.
//!
//! A process records its parent's PID, but the parent may have exited and its PID been
//! given to an unrelated process. A candidate parent is only accepted if it was
//! created no later than the child.

use crate::snapshot::{ProcessKey, ProcessSnapshot};

/// Longest chain followed, as a guard against cycles.
const MAX_DEPTH: usize = 64;

/// A process's identity and recorded parent PID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Node {
    pub key: ProcessKey,
    pub parent_pid: u32,
}

/// Ancestors of `start`, parent first, ending at the oldest still-running ancestor.
/// `lookup` finds the current process with a PID. The System Idle Process (PID 0) is
/// never included.
pub fn ancestor_chain(start: Node, lookup: impl Fn(u32) -> Option<Node>) -> Vec<ProcessKey> {
    let mut chain: Vec<ProcessKey> = Vec::new();
    let mut child = start;
    while chain.len() < MAX_DEPTH {
        let pid = child.parent_pid;
        if pid == 0 || pid == child.key.pid {
            break;
        }
        let Some(parent) = lookup(pid) else { break };
        let reused = parent.key.create_time > child.key.create_time;
        let cycle = parent.key == start.key || chain.contains(&parent.key);
        if reused || cycle {
            break;
        }
        chain.push(parent.key);
        child = parent;
    }
    chain
}

impl ProcessSnapshot {
    /// Ancestors of a process in this snapshot, parent first. Empty if the process is
    /// not in the snapshot.
    pub fn ancestors(&self, key: ProcessKey) -> Vec<ProcessKey> {
        let node = |e: &crate::snapshot::ProcessEntry| Node {
            key: e.key(),
            parent_pid: e.parent_pid,
        };
        match self.find(key.pid).filter(|e| e.key() == key) {
            Some(entry) => ancestor_chain(node(entry), |pid| self.find(pid).map(node)),
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn node(pid: u32, create_time: i64, parent_pid: u32) -> Node {
        Node {
            key: ProcessKey { pid, create_time },
            parent_pid,
        }
    }

    fn chain(nodes: &[Node], start_pid: u32) -> Vec<u32> {
        let map: HashMap<u32, Node> = nodes.iter().map(|n| (n.key.pid, *n)).collect();
        ancestor_chain(map[&start_pid], |pid| map.get(&pid).copied())
            .iter()
            .map(|k| k.pid)
            .collect()
    }

    #[test]
    fn walks_to_the_root() {
        let nodes = [
            node(4, 10, 0),       // System
            node(600, 20, 4),     // smss.exe
            node(1000, 30, 600),  // explorer.exe (parent pid recorded as smss for test)
            node(2000, 40, 1000), // Teams.exe
            node(3000, 50, 2000), // msedgewebview2.exe
        ];
        assert_eq!(chain(&nodes, 3000), [2000, 1000, 600, 4]);
    }

    #[test]
    fn stops_when_parent_exited() {
        let nodes = [node(1000, 30, 999), node(2000, 40, 1000)];
        assert_eq!(chain(&nodes, 2000), [1000]);
    }

    #[test]
    fn stops_at_reused_parent_pid() {
        // 2000's real parent (PID 1000) exited; an unrelated process created later
        // now has PID 1000.
        let nodes = [node(1000, 99, 4), node(2000, 40, 1000), node(4, 10, 0)];
        assert_eq!(chain(&nodes, 2000), Vec::<u32>::new());
    }

    #[test]
    fn accepts_parent_created_at_the_same_tick() {
        let nodes = [node(1000, 40, 0), node(2000, 40, 1000)];
        assert_eq!(chain(&nodes, 2000), [1000]);
    }

    #[test]
    fn stops_on_cycles() {
        let nodes = [node(10, 5, 20), node(20, 5, 10)];
        assert_eq!(chain(&nodes, 10), [20]);
    }

    #[test]
    fn excludes_system_idle_process() {
        let nodes = [node(0, 0, 0), node(4, 10, 0)];
        assert_eq!(chain(&nodes, 4), Vec::<u32>::new());
    }

    #[test]
    fn live_snapshot_has_ancestors_for_this_process() {
        let snapshot = ProcessSnapshot::new().unwrap();
        let me = snapshot.find(std::process::id()).unwrap().key();
        // The test runner was started by something still running (cargo).
        assert!(!snapshot.ancestors(me).is_empty());
    }
}
