use std::collections::{HashMap, HashSet};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

pub type Identity = (u32, u64);

#[derive(Clone, Copy, Debug)]
pub struct ProcessRef {
    pub pid: u32,
    pub parent: Option<u32>,
    pub start_time: u64,
    pub shared: bool,
}

impl ProcessRef {
    fn identity(&self) -> Identity {
        (self.pid, self.start_time)
    }
}

#[derive(Default)]
struct Ownership {
    thread_id: Option<String>,
    explicit: bool,
    attempts: u8,
    parent_pid: Option<u32>,
    parent_identity: Option<Identity>,
    detached: bool,
}

#[derive(Default)]
pub struct OwnershipTracker {
    known: HashMap<Identity, Ownership>,
    last_error: Option<String>,
}

impl OwnershipTracker {
    /// Attribute only processes with an explicit thread UUID or a verified process ancestor.
    pub fn sample(&mut self, processes: &[ProcessRef]) -> HashMap<Identity, String> {
        self.sample_with(processes, read_thread_ids)
    }

    /// Per-process access failures remain unknown; sysinfo only exposes system initialization errors.
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    fn sample_with(
        &mut self,
        processes: &[ProcessRef],
        read: impl FnOnce(&[Identity]) -> Result<HashMap<Identity, String>, String>,
    ) -> HashMap<Identity, String> {
        let by_pid: HashMap<_, _> = processes.iter().map(|p| (p.pid, p)).collect();
        let live: HashSet<_> = processes.iter().map(ProcessRef::identity).collect();
        self.known.retain(|identity, _| live.contains(identity));
        let mut candidates = Vec::new();
        for process in processes {
            let parent = valid_parent(process, &by_pid).map(ProcessRef::identity);
            let ownership = self.known.entry(process.identity()).or_default();
            refresh_link(ownership, process, parent);
            if !process.shared
                && process.start_time > 0
                && !ownership.explicit
                && ownership.attempts < 2
            {
                ownership.attempts += 1;
                candidates.push(process.identity());
            }
        }
        if !candidates.is_empty() {
            self.read_candidates(&candidates, read);
        }
        let resolved = resolve_owners(processes, &by_pid, &self.known);
        let mut owners = HashMap::new();
        for (identity, thread_id) in resolved {
            self.known.get_mut(&identity).unwrap().thread_id = thread_id.clone();
            if let Some(thread_id) = thread_id {
                owners.insert(identity, thread_id);
            }
        }
        owners
    }

    fn read_candidates(
        &mut self,
        candidates: &[Identity],
        read: impl FnOnce(&[Identity]) -> Result<HashMap<Identity, String>, String>,
    ) {
        match read(candidates) {
            Ok(markers) => {
                self.last_error = None;
                for identity in candidates {
                    let Some(thread_id) = markers.get(identity).and_then(|s| thread_uuid(s)) else {
                        continue;
                    };
                    let ownership = self.known.get_mut(identity).unwrap();
                    ownership.thread_id = Some(thread_id);
                    ownership.explicit = true;
                }
            }
            Err(error) => self.last_error = Some(error),
        }
    }
}

/// Preserve observed ownership after reparenting, without treating a reused parent PID as ancestry.
fn refresh_link(ownership: &mut Ownership, process: &ProcessRef, parent: Option<Identity>) {
    if process.shared {
        ownership.thread_id = None;
        ownership.explicit = false;
        ownership.attempts = 2;
        ownership.detached = false;
    } else if ownership.thread_id.is_some()
        && (ownership.parent_pid != process.parent
            || matches!((ownership.parent_identity, parent), (Some(old), Some(new)) if old != new))
    {
        ownership.detached = true;
    }
    ownership.parent_pid = process.parent;
    if parent.is_some() || ownership.thread_id.is_none() {
        ownership.parent_identity = parent;
    }
}

/// Parents must be present in this sample and must not have started after their children.
fn valid_parent<'a>(
    process: &ProcessRef,
    by_pid: &HashMap<u32, &'a ProcessRef>,
) -> Option<&'a ProcessRef> {
    let parent = *by_pid.get(&process.parent?)?;
    (parent.pid != process.pid && parent.start_time > 0 && parent.start_time <= process.start_time)
        .then_some(parent)
}

/// Resolve each ancestry chain once; shared processes terminate the chain without an owner.
fn resolve_owners(
    processes: &[ProcessRef],
    by_pid: &HashMap<u32, &ProcessRef>,
    known: &HashMap<Identity, Ownership>,
) -> HashMap<Identity, Option<String>> {
    let mut resolved = HashMap::with_capacity(processes.len());
    for process in processes {
        let ownership = &known[&process.identity()];
        if process.shared {
            resolved.insert(process.identity(), None);
        } else if ownership.explicit || ownership.detached {
            resolved.insert(process.identity(), ownership.thread_id.clone());
        }
    }
    for process in processes {
        let mut current = process;
        let mut path = Vec::new();
        let mut visited = HashSet::new();
        let thread_id = loop {
            let identity = current.identity();
            if let Some(owner) = resolved.get(&identity) {
                break owner.clone();
            }
            if !visited.insert(identity) {
                break None;
            }
            path.push(identity);
            let Some(parent) = valid_parent(current, by_pid) else {
                break known[&identity].thread_id.clone();
            };
            current = parent;
        };
        for identity in path {
            resolved.insert(identity, thread_id.clone());
        }
    }
    resolved
}

/// Read environment blocks only in this temporary System, retaining just the thread UUID.
fn read_thread_ids(candidates: &[Identity]) -> Result<HashMap<Identity, String>, String> {
    let mut system = System::new().map_err(|error| format!("无法读取会话进程标识：{error}"))?;
    let pids: Vec<_> = candidates
        .iter()
        .map(|(pid, _)| Pid::from_u32(*pid))
        .collect();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&pids),
        true,
        ProcessRefreshKind::nothing()
            .without_tasks()
            .with_environ(UpdateKind::Always),
    );
    let mut markers = HashMap::new();
    for identity in candidates {
        let Some(process) = system.process(Pid::from_u32(identity.0)) else {
            continue;
        };
        if process.start_time() != identity.1 {
            continue;
        }
        if let Some(thread_id) = process.environ().iter().find_map(|entry| {
            let value = entry.to_str()?.strip_prefix("CODEX_THREAD_ID=")?;
            thread_uuid(value)
        }) {
            markers.insert(*identity, thread_id);
        }
    }
    Ok(markers)
}

/// Only canonical UUID text can identify a thread; session IDs and other environment stay unused.
fn thread_uuid(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    if bytes.len() != 36 {
        return None;
    }
    for (index, byte) in bytes.iter().enumerate() {
        let valid = if matches!(index, 8 | 13 | 18 | 23) {
            *byte == b'-'
        } else {
            byte.is_ascii_hexdigit()
        };
        if !valid {
            return None;
        }
    }
    Some(value.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIRST: &str = "019a7f83-12ba-76de-8a12-3456789abcde";
    const SECOND: &str = "019a7f83-12ba-76de-8b12-3456789abcdf";

    fn process(pid: u32, parent: Option<u32>, start_time: u64) -> ProcessRef {
        ProcessRef {
            pid,
            parent,
            start_time,
            shared: false,
        }
    }

    fn markers(values: &[(Identity, &str)]) -> HashMap<Identity, String> {
        values
            .iter()
            .map(|(id, value)| (*id, (*value).into()))
            .collect()
    }

    // Two sessions share one application root; tools inherit only their session's UUID.
    #[test]
    fn isolates_sessions_and_shared_root() {
        let mut root = process(1, None, 1);
        root.shared = true;
        let processes = [
            root,
            process(10, Some(1), 2),
            process(11, Some(10), 3),
            process(20, Some(1), 2),
            process(21, Some(20), 3),
            process(30, Some(1), 3),
        ];
        let owners = OwnershipTracker::default().sample_with(&processes, |candidates| {
            assert!(!candidates.contains(&(1, 1)));
            Ok(markers(&[
                ((1, 1), FIRST),
                ((10, 2), FIRST),
                ((20, 2), SECOND),
            ]))
        });
        assert_eq!(owners[&(11, 3)], FIRST);
        assert_eq!(owners[&(21, 3)], SECOND);
        assert!(!owners.contains_key(&(1, 1)));
        assert!(!owners.contains_key(&(30, 3)));
    }

    // A shared intermediary blocks ownership above it, while a tool's own UUID remains valid.
    #[test]
    fn shared_process_blocks_ancestor_inheritance() {
        let mut shared = process(2, Some(1), 2);
        shared.shared = true;
        let processes = [
            process(1, None, 1),
            shared,
            process(3, Some(2), 3),
            process(4, Some(2), 3),
        ];
        let owners = OwnershipTracker::default().sample_with(&processes, |_| {
            Ok(markers(&[((1, 1), FIRST), ((4, 3), SECOND)]))
        });
        assert!(!owners.contains_key(&(2, 2)));
        assert!(!owners.contains_key(&(3, 3)));
        assert_eq!(owners[&(4, 3)], SECOND);
    }

    // A child agent carries a different UUID, which overrides its parent's session for all its tools.
    #[test]
    fn explicit_child_overrides_parent() {
        let processes = [
            process(1, None, 1),
            process(2, Some(1), 2),
            process(3, Some(2), 3),
        ];
        let owners = OwnershipTracker::default().sample_with(&processes, |_| {
            Ok(markers(&[((1, 1), FIRST), ((2, 2), SECOND)]))
        });
        assert_eq!(owners[&(1, 1)], FIRST);
        assert_eq!(owners[&(2, 2)], SECOND);
        assert_eq!(owners[&(3, 3)], SECOND);
    }

    // A missing first environment read is retried once; a later explicit UUID corrects descendants.
    #[test]
    fn retry_corrects_inherited_ownership() {
        let processes = [
            process(1, None, 1),
            process(2, Some(1), 2),
            process(3, Some(2), 3),
        ];
        let mut tracker = OwnershipTracker::default();
        let first = tracker.sample_with(&processes, |_| Ok(markers(&[((1, 1), FIRST)])));
        assert_eq!(first[&(3, 3)], FIRST);
        let second = tracker.sample_with(&processes, |candidates| {
            assert_eq!(candidates, &[(2, 2), (3, 3)]);
            Ok(markers(&[((2, 2), SECOND)]))
        });
        assert_eq!(second[&(2, 2)], SECOND);
        assert_eq!(second[&(3, 3)], SECOND);
        tracker.sample_with(&processes, |_| {
            panic!("environment retried more than twice")
        });
    }

    // A tool that is adopted by another process keeps the already observed session ownership.
    #[test]
    fn reparented_process_retains_ownership() {
        let mut tracker = OwnershipTracker::default();
        let mut processes = [
            process(1, None, 1),
            process(2, Some(1), 2),
            process(3, None, 1),
        ];
        tracker.sample_with(&processes, |_| {
            Ok(markers(&[((1, 1), FIRST), ((3, 1), SECOND)]))
        });
        processes[1].parent = Some(3);
        let owners = tracker.sample_with(&processes, |_| Ok(HashMap::new()));
        assert_eq!(owners[&(2, 2)], FIRST);
        let owners = tracker.sample_with(&processes[1..2], |_| panic!("unexpected new probe"));
        assert_eq!(owners[&(2, 2)], FIRST);
    }

    // A reused PID has a new start time and cannot inherit the old process's cached session.
    #[test]
    fn pid_reuse_drops_old_ownership() {
        let mut tracker = OwnershipTracker::default();
        tracker.sample_with(&[process(1, None, 1)], |_| Ok(markers(&[((1, 1), FIRST)])));
        let owners = tracker.sample_with(&[process(1, None, 2)], |candidates| {
            assert_eq!(candidates, &[(1, 2)]);
            Ok(HashMap::new())
        });
        assert!(owners.is_empty());
        assert_eq!(tracker.known.len(), 1);
        assert!(!tracker.known.contains_key(&(1, 1)));
    }

    // A newly observed child cannot inherit from a reused parent PID that started after it.
    #[test]
    fn rejects_newer_parent_identity() {
        let processes = [process(1, None, 4), process(2, Some(1), 2)];
        let owners = OwnershipTracker::default()
            .sample_with(&processes, |_| Ok(markers(&[((1, 4), FIRST)])));
        assert_eq!(owners[&(1, 4)], FIRST);
        assert!(!owners.contains_key(&(2, 2)));
    }

    // Failed native reads receive one retry, then remain unknown without repeated environment work.
    #[test]
    fn failed_reads_are_bounded() {
        let mut tracker = OwnershipTracker::default();
        let processes = [process(1, None, 1)];
        for _ in 0..2 {
            assert!(tracker
                .sample_with(&processes, |_| Err("unavailable".into()))
                .is_empty());
        }
        assert_eq!(tracker.last_error(), Some("unavailable"));
        assert!(tracker
            .sample_with(&processes, |_| panic!("retry limit exceeded"))
            .is_empty());
    }

    // Only UUID-shaped thread markers are accepted, and uppercase UUIDs normalize consistently.
    #[test]
    fn accepts_only_canonical_uuid_text() {
        assert_eq!(thread_uuid(&FIRST.to_uppercase()).as_deref(), Some(FIRST));
        for invalid in [
            "session-123",
            "",
            "019a7f83_12ba-76de-8a12-3456789abcde",
            "{019a7f83-12ba-76de-8a12-3456789abcde}",
        ] {
            assert!(thread_uuid(invalid).is_none());
        }
    }
}
