use crate::contacts::{Contact, Schedule};
use hardy_bpa::routes::{Action, RoutingSink};
use hardy_eid_patterns::EidPattern;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use time::OffsetDateTime;
use tokio::sync::oneshot;
use tracing::{debug, info, warn};

// ── Public types ────────────────────────────────────────────────────

#[derive(Debug)]
pub struct AddResult {
    pub added: u32,
    pub active: u32,
    pub skipped: u32,
}

#[derive(Debug)]
pub struct RemoveResult {
    pub removed: u32,
}

#[derive(Debug)]
pub struct ReplaceResult {
    pub added: u32,
    pub removed: u32,
    pub unchanged: u32,
}

// ── Commands ────────────────────────────────────────────────────────

enum Command {
    Add {
        source: String,
        contacts: Vec<Contact>,
        default_priority: u32,
        reply: oneshot::Sender<AddResult>,
    },
    Remove {
        source: String,
        contacts: Vec<Contact>,
        reply: oneshot::Sender<RemoveResult>,
    },
    Replace {
        source: String,
        contacts: Vec<Contact>,
        default_priority: u32,
        reply: oneshot::Sender<ReplaceResult>,
    },
    WithdrawAll {
        source: String,
    },
}

// ── Handle ──────────────────────────────────────────────────────────

/// Cloneable handle for sending commands to the scheduler.
#[derive(Clone)]
pub struct SchedulerHandle {
    tx: flume::Sender<Command>,
}

impl SchedulerHandle {
    pub async fn add_contacts(
        &self,
        source: &str,
        contacts: Vec<Contact>,
        default_priority: u32,
    ) -> Option<AddResult> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send_async(Command::Add {
                source: source.to_string(),
                contacts,
                default_priority,
                reply,
            })
            .await
            .ok()?;
        rx.await.ok()
    }

    pub async fn remove_contacts(
        &self,
        source: &str,
        contacts: Vec<Contact>,
    ) -> Option<RemoveResult> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send_async(Command::Remove {
                source: source.to_string(),
                contacts,
                reply,
            })
            .await
            .ok()?;
        rx.await.ok()
    }

    pub async fn replace_contacts(
        &self,
        source: &str,
        contacts: Vec<Contact>,
        default_priority: u32,
    ) -> Option<ReplaceResult> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send_async(Command::Replace {
                source: source.to_string(),
                contacts,
                default_priority,
                reply,
            })
            .await
            .ok()?;
        rx.await.ok()
    }

    pub async fn withdraw_all(&self, source: &str) {
        let _ = self
            .tx
            .send_async(Command::WithdrawAll {
                source: source.to_string(),
            })
            .await;
    }
}

/// The receive side of the scheduler channel (opaque).
pub struct SchedulerReceiver {
    rx: flume::Receiver<Command>,
}

/// Create a scheduler handle/receiver pair.
pub fn channel() -> (SchedulerHandle, SchedulerReceiver) {
    let (tx, rx) = flume::unbounded();
    (SchedulerHandle { tx }, SchedulerReceiver { rx })
}

// ── Scheduler internals ─────────────────────────────────────────────

/// Route identity for refcounting — two contacts produce the same route
/// if they match on these three fields.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RouteKey {
    pattern: EidPattern,
    action: Action,
    priority: u32,
}

/// A contact managed by the scheduler, with resolved priority and state.
struct ManagedContact {
    contact: Contact,
    priority: u32,
    active: bool,
}

impl ManagedContact {
    fn route_key(&self) -> RouteKey {
        RouteKey {
            pattern: self.contact.pattern.clone(),
            action: self.contact.action.clone(),
            priority: self.priority,
        }
    }
}

/// Ordered: Deactivate (0) before Activate (1) at the same timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum EventKind {
    Deactivate,
    Activate,
}

/// A scheduled event. `Ord` gives: time ascending, then Deactivate before
/// Activate, then by contact ID for determinism.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Event {
    time: OffsetDateTime,
    kind: EventKind,
    contact_id: u64,
}

impl Ord for Event {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.time
            .cmp(&other.time)
            .then(self.kind.cmp(&other.kind))
            .then(self.contact_id.cmp(&other.contact_id))
    }
}

impl PartialOrd for Event {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

struct Scheduler {
    /// Source label → set of contact IDs from that source
    sources: HashMap<String, HashSet<u64>>,
    /// All managed contacts by ID
    contacts: HashMap<u64, ManagedContact>,
    /// Event timeline — ordered by (time, kind, contact_id)
    timeline: BTreeSet<Event>,
    /// Route refcounts — how many active contacts provide each route
    route_refs: HashMap<RouteKey, u32>,
    /// Next contact ID
    next_id: u64,
    /// BPA routing sink
    sink: Arc<dyn RoutingSink>,
    /// Task pool for spawning route operations
    tasks: hardy_async::TaskPool,
}

impl Scheduler {
    fn new(sink: Arc<dyn RoutingSink>, tasks: hardy_async::TaskPool) -> Self {
        Self {
            sources: HashMap::new(),
            contacts: HashMap::new(),
            timeline: BTreeSet::new(),
            route_refs: HashMap::new(),
            next_id: 0,
            sink,
            tasks,
        }
    }

    /// Resolve a contact's priority from its own value or the default.
    fn resolve_priority(contact: &Contact, default: u32) -> u32 {
        contact.priority.unwrap_or(default)
    }

    /// Allocate a new contact ID.
    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    // ── Ingestion ───────────────────────────────────────────────────

    /// Ingest a single contact: create ManagedContact, schedule events,
    /// activate if currently in window. Returns (added, active, skipped).
    fn ingest(
        &mut self,
        source: &str,
        contact: Contact,
        default_priority: u32,
        now: OffsetDateTime,
    ) -> (bool, bool) {
        let priority = Self::resolve_priority(&contact, default_priority);
        let id = self.alloc_id();

        let mc = ManagedContact {
            contact,
            priority,
            active: false,
        };

        let (added, activated) = match &mc.contact.schedule {
            Schedule::Permanent => {
                // Activate immediately, no deactivation event
                self.contacts.insert(id, mc);
                self.sources
                    .entry(source.to_string())
                    .or_default()
                    .insert(id);
                (true, true)
            }
            Schedule::OneShot { start, end } => {
                let start = start.unwrap_or(now);
                let end = *end;

                // Skip if entirely in the past
                if let Some(end) = end
                    && end <= now
                {
                    return (false, false);
                }

                self.contacts.insert(id, mc);
                self.sources
                    .entry(source.to_string())
                    .or_default()
                    .insert(id);

                if start <= now {
                    // Currently active
                    if let Some(end) = end {
                        self.insert_event(end, EventKind::Deactivate, id);
                    }
                    (true, true)
                } else {
                    // Future
                    self.insert_event(start, EventKind::Activate, id);
                    if let Some(end) = end {
                        self.insert_event(end, EventKind::Deactivate, id);
                    }
                    (true, false)
                }
            }
            Schedule::Recurring {
                cron,
                duration,
                until,
            } => {
                let duration = *duration;
                let until = *until;

                // Find the current or next occurrence
                // First check: are we inside an active occurrence right now?
                let active_now = if let Some(prev_start) = cron.prev_before(now) {
                    let prev_end = prev_start + duration;
                    if prev_end > now && until.is_none_or(|u| prev_start < u) {
                        Some((prev_start, prev_end))
                    } else {
                        None
                    }
                } else {
                    None
                };

                self.contacts.insert(id, mc);
                self.sources
                    .entry(source.to_string())
                    .or_default()
                    .insert(id);

                if let Some((_start, end)) = active_now {
                    // Currently in an active occurrence — schedule deactivate
                    self.insert_event(end, EventKind::Deactivate, id);
                    // Next occurrence will be scheduled when this one deactivates
                    (true, true)
                } else {
                    // Schedule next future occurrence
                    self.schedule_next_occurrence(id, now);
                    (true, false)
                }
            }
        };

        if activated {
            self.activate_contact(id);
        }

        (added, activated)
    }

    /// Schedule the next occurrence of a recurring contact after `after`.
    fn schedule_next_occurrence(&mut self, id: u64, after: OffsetDateTime) {
        let mc = match self.contacts.get(&id) {
            Some(mc) => mc,
            None => return,
        };

        let (cron, duration, until) = match &mc.contact.schedule {
            Schedule::Recurring {
                cron,
                duration,
                until,
            } => (cron, *duration, *until),
            _ => return,
        };

        let next_start = match cron.next_after(after) {
            Some(t) => t,
            None => return, // no more occurrences
        };

        // Check until bound
        if let Some(until) = until
            && next_start >= until
        {
            return;
        }

        let next_end = next_start + duration;

        self.insert_event(next_start, EventKind::Activate, id);
        self.insert_event(next_end, EventKind::Deactivate, id);
    }

    // ── Event timeline ──────────────────────────────────────────────

    fn insert_event(&mut self, time: OffsetDateTime, kind: EventKind, contact_id: u64) {
        self.timeline.insert(Event {
            time,
            kind,
            contact_id,
        });
    }

    /// Remove all pending events for a contact.
    fn cancel_events(&mut self, contact_id: u64) {
        self.timeline.retain(|e| e.contact_id != contact_id);
    }

    fn next_event_time(&self) -> Option<OffsetDateTime> {
        self.timeline.first().map(|e| e.time)
    }

    // ── Route activation / deactivation ─────────────────────────────

    fn activate_contact(&mut self, id: u64) {
        let mc = match self.contacts.get_mut(&id) {
            Some(mc) => mc,
            None => return,
        };
        if mc.active {
            return;
        }
        mc.active = true;
        let key = mc.route_key();
        let count = self.route_refs.entry(key).or_insert(0);
        *count += 1;
        // First activation → install route
        if *count == 1 {
            let mc = &self.contacts[&id];
            let pattern = mc.contact.pattern.clone();
            let action = mc.contact.action.clone();
            let priority = mc.priority;
            let sink = self.sink.clone();
            hardy_async::spawn!(self.tasks, "add_route", async move {
                if let Err(e) = sink.add_route(pattern, action, priority).await {
                    warn!("Failed to add route: {e}");
                }
            });
        }
    }

    fn deactivate_contact(&mut self, id: u64) {
        let mc = match self.contacts.get_mut(&id) {
            Some(mc) => mc,
            None => return,
        };
        if !mc.active {
            return;
        }
        mc.active = false;
        let key = mc.route_key();
        if let Some(count) = self.route_refs.get_mut(&key) {
            *count -= 1;
            if *count == 0 {
                self.route_refs.remove(&key);
                // Last deactivation → remove route
                let pattern = mc.contact.pattern.clone();
                let action = mc.contact.action.clone();
                let priority = mc.priority;
                let sink = self.sink.clone();
                hardy_async::spawn!(self.tasks, "remove_route", async move {
                    if let Err(e) = sink.remove_route(&pattern, &action, priority).await {
                        warn!("Failed to remove route: {e}");
                    }
                });
            }
        }
    }

    // ── Removal ─────────────────────────────────────────────────────

    /// Remove a contact by ID: cancel events, deactivate if active, clean up.
    fn remove_contact(&mut self, id: u64) {
        self.cancel_events(id);
        self.deactivate_contact(id);
        self.contacts.remove(&id);
    }

    /// Remove all contacts for a source.
    fn withdraw_source(&mut self, source: &str) {
        if let Some(ids) = self.sources.remove(source) {
            for id in ids {
                self.remove_contact(id);
            }
        }
    }

    // ── Command handlers ────────────────────────────────────────────

    fn handle_add(
        &mut self,
        source: &str,
        contacts: Vec<Contact>,
        default_priority: u32,
        now: OffsetDateTime,
    ) -> AddResult {
        let mut added = 0u32;
        let mut active = 0u32;
        let mut skipped = 0u32;

        for contact in contacts {
            let (was_added, was_active) = self.ingest(source, contact, default_priority, now);
            if was_added {
                added += 1;
                if was_active {
                    active += 1;
                }
            } else {
                skipped += 1;
            }
        }

        debug!("Add for '{source}': added={added}, active={active}, skipped={skipped}");
        AddResult {
            added,
            active,
            skipped,
        }
    }

    fn handle_remove(&mut self, source: &str, contacts: Vec<Contact>) -> RemoveResult {
        let mut removed = 0u32;

        if let Some(ids) = self.sources.get(source) {
            let ids_snapshot: Vec<u64> = ids.iter().copied().collect();
            for id in ids_snapshot {
                let mc = match self.contacts.get(&id) {
                    Some(mc) => mc,
                    None => continue,
                };
                // Match by contact content (pattern + action + schedule)
                if contacts.iter().any(|c| contacts_match(c, &mc.contact)) {
                    self.remove_contact(id);
                    if let Some(ids) = self.sources.get_mut(source) {
                        ids.remove(&id);
                    }
                    removed += 1;
                }
            }
        }

        debug!("Remove for '{source}': removed={removed}");
        RemoveResult { removed }
    }

    fn handle_replace(
        &mut self,
        source: &str,
        contacts: Vec<Contact>,
        default_priority: u32,
        now: OffsetDateTime,
    ) -> ReplaceResult {
        // Snapshot the current contacts for this source
        let old_ids: Vec<u64> = self
            .sources
            .get(source)
            .map(|ids| ids.iter().copied().collect())
            .unwrap_or_default();

        let old_contacts: Vec<(u64, Contact)> = old_ids
            .iter()
            .filter_map(|id| self.contacts.get(id).map(|mc| (*id, mc.contact.clone())))
            .collect();

        // Compute diff
        let mut unchanged = 0u32;
        let mut to_remove: Vec<u64> = Vec::new();
        let mut to_add: Vec<Contact> = Vec::new();

        // Find old contacts not in new set → remove
        for (id, old_contact) in &old_contacts {
            if !contacts.iter().any(|c| contacts_match(c, old_contact)) {
                to_remove.push(*id);
            } else {
                unchanged += 1;
            }
        }

        // Find new contacts not in old set → add
        for contact in contacts {
            if !old_contacts
                .iter()
                .any(|(_, old)| contacts_match(&contact, old))
            {
                to_add.push(contact);
            }
        }

        // Apply removals
        for id in &to_remove {
            self.remove_contact(*id);
            if let Some(ids) = self.sources.get_mut(source) {
                ids.remove(id);
            }
        }

        // Apply additions
        let mut added = 0u32;
        for contact in to_add {
            let (was_added, _) = self.ingest(source, contact, default_priority, now);
            if was_added {
                added += 1;
            }
        }

        let removed = to_remove.len() as u32;
        debug!("Replace for '{source}': added={added}, removed={removed}, unchanged={unchanged}");
        ReplaceResult {
            added,
            removed,
            unchanged,
        }
    }

    // ── Event processing ────────────────────────────────────────────

    /// Process all events up to and including `now`.
    fn process_due_events(&mut self, now: OffsetDateTime) {
        while let Some(event) = self.timeline.first().cloned() {
            if event.time > now {
                break;
            }
            self.timeline.pop_first();

            match event.kind {
                EventKind::Activate => {
                    self.activate_contact(event.contact_id);
                }
                EventKind::Deactivate => {
                    self.deactivate_contact(event.contact_id);
                    // For recurring contacts, schedule the next occurrence
                    if let Some(mc) = self.contacts.get(&event.contact_id)
                        && matches!(mc.contact.schedule, Schedule::Recurring { .. })
                    {
                        self.schedule_next_occurrence(event.contact_id, event.time);
                    }
                }
            }
        }
    }
}

/// Two contacts match if they have the same pattern, action, and schedule.
/// Used for diffing in Replace and matching in Remove.
fn contacts_match(a: &Contact, b: &Contact) -> bool {
    a.pattern == b.pattern
        && a.action == b.action
        && a.schedule == b.schedule
        && a.priority == b.priority
        && a.bandwidth_bps == b.bandwidth_bps
        && a.delay_us == b.delay_us
}

// ── Core loop ───────────────────────────────────────────────────────

/// Start the scheduler task.
pub fn start(
    receiver: SchedulerReceiver,
    sink: Arc<dyn RoutingSink>,
    tasks: &hardy_async::TaskPool,
) {
    let rx = receiver.rx;
    let cancel = tasks.cancel_token().clone();
    let sched_tasks = tasks.clone();

    hardy_async::spawn!(tasks, "tvr_scheduler", async move {
        let mut sched = Scheduler::new(sink, sched_tasks);

        info!("Scheduler started");

        loop {
            let wake_at = match sched.next_event_time() {
                Some(t) => {
                    let now = OffsetDateTime::now_utc();
                    let delay = (t - now).max(time::Duration::ZERO);
                    tokio::time::Instant::now()
                        + std::time::Duration::try_from(delay).unwrap_or(std::time::Duration::ZERO)
                }
                None => tokio::time::Instant::now() + std::time::Duration::from_secs(86400 * 365),
            };

            tokio::select! {
                _ = tokio::time::sleep_until(wake_at) => {
                    let now = OffsetDateTime::now_utc();
                    sched.process_due_events(now);
                }
                cmd = rx.recv_async() => {
                    match cmd {
                        Ok(cmd) => {
                            let now = OffsetDateTime::now_utc();
                            match cmd {
                                Command::Add { source, contacts, default_priority, reply } => {
                                    let result = sched.handle_add(&source, contacts, default_priority, now);
                                    let _ = reply.send(result);
                                }
                                Command::Remove { source, contacts, reply } => {
                                    let result = sched.handle_remove(&source, contacts);
                                    let _ = reply.send(result);
                                }
                                Command::Replace { source, contacts, default_priority, reply } => {
                                    let result = sched.handle_replace(&source, contacts, default_priority, now);
                                    let _ = reply.send(result);
                                }
                                Command::WithdrawAll { source } => {
                                    sched.withdraw_source(&source);
                                }
                            }
                        }
                        Err(_) => {
                            info!("Scheduler channel closed, shutting down");
                            break;
                        }
                    }
                }
                _ = cancel.cancelled() => {
                    info!("Scheduler cancelled");
                    break;
                }
            }
        }
    });
}

#[cfg(test)]
mod test {
    use super::*;
    use hardy_bpa::routes;
    use time::macros::datetime;

    // ── Mock sink ───────────────────────────────────────────────────

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum RouteOp {
        Add {
            pattern: EidPattern,
            action: Action,
            priority: u32,
        },
        Remove {
            pattern: EidPattern,
            action: Action,
            priority: u32,
        },
    }

    struct MockSink {
        tx: flume::Sender<RouteOp>,
    }

    impl MockSink {
        fn new() -> (Arc<Self>, flume::Receiver<RouteOp>) {
            let (tx, rx) = flume::unbounded();
            (Arc::new(Self { tx }), rx)
        }
    }

    #[hardy_bpa::async_trait]
    impl RoutingSink for MockSink {
        async fn unregister(&self) {}

        async fn add_route(
            &self,
            pattern: EidPattern,
            action: Action,
            priority: u32,
        ) -> routes::Result<bool> {
            let _ = self.tx.send(RouteOp::Add {
                pattern,
                action,
                priority,
            });
            Ok(true)
        }

        async fn remove_route(
            &self,
            pattern: &EidPattern,
            action: &Action,
            priority: u32,
        ) -> routes::Result<bool> {
            let _ = self.tx.send(RouteOp::Remove {
                pattern: pattern.clone(),
                action: action.clone(),
                priority,
            });
            Ok(true)
        }
    }

    // ── Helpers ─────────────────────────────────────────────────────

    fn make_sched(sink: Arc<dyn RoutingSink>) -> Scheduler {
        Scheduler::new(sink, hardy_async::TaskPool::new())
    }

    /// Yield to let spawned route tasks complete.
    async fn flush() {
        tokio::task::yield_now().await;
    }

    /// Drain all pending route operations from the mock.
    fn drain_ops(rx: &flume::Receiver<RouteOp>) -> Vec<RouteOp> {
        let mut ops = Vec::new();
        while let Ok(op) = rx.try_recv() {
            ops.push(op);
        }
        ops
    }

    fn via(next_hop: &str) -> Action {
        Action::Via(next_hop.parse().unwrap())
    }

    fn pat(s: &str) -> EidPattern {
        s.parse().unwrap()
    }

    fn permanent_contact(pattern: &str, next_hop: &str) -> Contact {
        Contact {
            pattern: pat(pattern),
            action: via(next_hop),
            priority: None,
            schedule: Schedule::Permanent,
            bandwidth_bps: None,
            delay_us: None,
        }
    }

    fn oneshot_contact(
        pattern: &str,
        next_hop: &str,
        start: Option<OffsetDateTime>,
        end: Option<OffsetDateTime>,
    ) -> Contact {
        Contact {
            pattern: pat(pattern),
            action: via(next_hop),
            priority: None,
            schedule: Schedule::OneShot { start, end },
            bandwidth_bps: None,
            delay_us: None,
        }
    }

    fn recurring_contact(
        pattern: &str,
        next_hop: &str,
        cron: &str,
        duration: std::time::Duration,
        until: Option<OffsetDateTime>,
    ) -> Contact {
        Contact {
            pattern: pat(pattern),
            action: via(next_hop),
            priority: None,
            schedule: Schedule::Recurring {
                cron: crate::cron::CronExpr::parse(cron).unwrap(),
                duration,
                until,
            },
            bandwidth_bps: None,
            delay_us: None,
        }
    }

    // ── Permanent contacts ──────────────────────────────────────────

    #[tokio::test]
    async fn permanent_activates_immediately() {
        let (sink, rx) = MockSink::new();
        let mut sched = make_sched(sink);

        let now = datetime!(2026-03-27 08:00:00 UTC);
        let result = sched.handle_add(
            "src",
            vec![permanent_contact("ipn:2.*.*", "ipn:2.1.0")],
            100,
            now,
        );

        assert_eq!(result.added, 1);
        assert_eq!(result.active, 1);
        assert_eq!(result.skipped, 0);
        assert!(sched.timeline.is_empty()); // no events — always active

        flush().await;
        let ops = drain_ops(&rx);
        assert_eq!(ops.len(), 1);
        assert_eq!(
            ops[0],
            RouteOp::Add {
                pattern: pat("ipn:2.*.*"),
                action: via("ipn:2.1.0"),
                priority: 100,
            }
        );
    }

    #[tokio::test]
    async fn permanent_with_explicit_priority() {
        let (sink, rx) = MockSink::new();
        let mut sched = make_sched(sink);

        let mut contact = permanent_contact("ipn:2.*.*", "ipn:2.1.0");
        contact.priority = Some(42);

        sched.handle_add(
            "src",
            vec![contact],
            100,
            datetime!(2026-03-27 08:00:00 UTC),
        );

        flush().await;
        let ops = drain_ops(&rx);
        assert_eq!(
            ops[0],
            RouteOp::Add {
                pattern: pat("ipn:2.*.*"),
                action: via("ipn:2.1.0"),
                priority: 42, // explicit, not default 100
            }
        );
    }

    // ── One-shot contacts ───────────────────────────────────────────

    #[tokio::test]
    async fn oneshot_future_schedules_events() {
        let (sink, rx) = MockSink::new();
        let mut sched = make_sched(sink);

        let now = datetime!(2026-03-27 08:00:00 UTC);
        let start = datetime!(2026-03-27 10:00:00 UTC);
        let end = datetime!(2026-03-27 11:00:00 UTC);

        let result = sched.handle_add(
            "src",
            vec![oneshot_contact(
                "ipn:2.*.*",
                "ipn:2.1.0",
                Some(start),
                Some(end),
            )],
            100,
            now,
        );

        assert_eq!(result.added, 1);
        assert_eq!(result.active, 0);
        assert_eq!(sched.timeline.len(), 2); // activate + deactivate

        flush().await;
        assert!(drain_ops(&rx).is_empty()); // not active yet
    }

    #[tokio::test]
    async fn oneshot_active_now() {
        let (sink, rx) = MockSink::new();
        let mut sched = make_sched(sink);

        let now = datetime!(2026-03-27 10:30:00 UTC);
        let start = datetime!(2026-03-27 10:00:00 UTC);
        let end = datetime!(2026-03-27 11:00:00 UTC);

        let result = sched.handle_add(
            "src",
            vec![oneshot_contact(
                "ipn:2.*.*",
                "ipn:2.1.0",
                Some(start),
                Some(end),
            )],
            100,
            now,
        );

        assert_eq!(result.added, 1);
        assert_eq!(result.active, 1);
        assert_eq!(sched.timeline.len(), 1); // deactivate only

        flush().await;
        let ops = drain_ops(&rx);
        assert_eq!(ops.len(), 1);
        assert!(matches!(ops[0], RouteOp::Add { .. }));
    }

    #[tokio::test]
    async fn oneshot_past_skipped() {
        let (sink, rx) = MockSink::new();
        let mut sched = make_sched(sink);

        let now = datetime!(2026-03-27 12:00:00 UTC);
        let start = datetime!(2026-03-27 10:00:00 UTC);
        let end = datetime!(2026-03-27 11:00:00 UTC);

        let result = sched.handle_add(
            "src",
            vec![oneshot_contact(
                "ipn:2.*.*",
                "ipn:2.1.0",
                Some(start),
                Some(end),
            )],
            100,
            now,
        );

        assert_eq!(result.added, 0);
        assert_eq!(result.skipped, 1);
        assert!(sched.timeline.is_empty());

        flush().await;
        assert!(drain_ops(&rx).is_empty());
    }

    #[tokio::test]
    async fn oneshot_no_start_activates_immediately() {
        let (sink, rx) = MockSink::new();
        let mut sched = make_sched(sink);

        let now = datetime!(2026-03-27 08:00:00 UTC);
        let end = datetime!(2026-03-27 11:00:00 UTC);

        let result = sched.handle_add(
            "src",
            vec![oneshot_contact("ipn:2.*.*", "ipn:2.1.0", None, Some(end))],
            100,
            now,
        );

        assert_eq!(result.active, 1);
        assert_eq!(sched.timeline.len(), 1); // deactivate only

        flush().await;
        assert_eq!(drain_ops(&rx).len(), 1);
    }

    #[tokio::test]
    async fn oneshot_no_end_stays_active() {
        let (sink, _rx) = MockSink::new();
        let mut sched = make_sched(sink);

        let now = datetime!(2026-03-27 08:00:00 UTC);
        let start = datetime!(2026-03-27 07:00:00 UTC);

        sched.handle_add(
            "src",
            vec![oneshot_contact("ipn:2.*.*", "ipn:2.1.0", Some(start), None)],
            100,
            now,
        );

        assert!(sched.timeline.is_empty()); // no deactivate event
    }

    // ── Event processing ────────────────────────────────────────────

    #[tokio::test]
    async fn events_fire_in_order() {
        let (sink, rx) = MockSink::new();
        let mut sched = make_sched(sink);

        let now = datetime!(2026-03-27 08:00:00 UTC);
        let start = datetime!(2026-03-27 10:00:00 UTC);
        let end = datetime!(2026-03-27 11:00:00 UTC);

        sched.handle_add(
            "src",
            vec![oneshot_contact(
                "ipn:2.*.*",
                "ipn:2.1.0",
                Some(start),
                Some(end),
            )],
            100,
            now,
        );

        // Process activation
        sched.process_due_events(start);
        flush().await;
        let ops = drain_ops(&rx);
        assert_eq!(ops.len(), 1);
        assert!(matches!(ops[0], RouteOp::Add { .. }));

        // Process deactivation
        sched.process_due_events(end);
        flush().await;
        let ops = drain_ops(&rx);
        assert_eq!(ops.len(), 1);
        assert!(matches!(ops[0], RouteOp::Remove { .. }));
    }

    #[tokio::test]
    async fn deactivate_before_activate_at_same_time() {
        let (sink, rx) = MockSink::new();
        let mut sched = make_sched(sink);

        let now = datetime!(2026-03-27 07:00:00 UTC);
        let t = datetime!(2026-03-27 10:00:00 UTC);

        // Contact A ends at t
        sched.handle_add(
            "src",
            vec![oneshot_contact(
                "ipn:2.*.*",
                "ipn:2.1.0",
                Some(now),
                Some(t),
            )],
            100,
            now,
        );
        flush().await;
        drain_ops(&rx); // clear the initial add

        // Contact B starts at t (same pattern, same route)
        let end_b = datetime!(2026-03-27 12:00:00 UTC);
        sched.handle_add(
            "src",
            vec![oneshot_contact(
                "ipn:2.*.*",
                "ipn:2.1.0",
                Some(t),
                Some(end_b),
            )],
            100,
            now,
        );

        // Process events at t — deactivate A then activate B
        sched.process_due_events(t);
        flush().await;
        let ops = drain_ops(&rx);
        // With refcounting: remove fires (1→0), then add fires (0→1)
        assert_eq!(ops.len(), 2);
        assert!(matches!(ops[0], RouteOp::Remove { .. }));
        assert!(matches!(ops[1], RouteOp::Add { .. }));
    }

    // ── Recurring contacts ──────────────────────────────────────────

    #[tokio::test]
    async fn recurring_schedules_next_occurrence() {
        let (sink, _rx) = MockSink::new();
        let mut sched = make_sched(sink);

        let now = datetime!(2026-03-27 07:00:00 UTC);

        sched.handle_add(
            "src",
            vec![recurring_contact(
                "ipn:2.*.*",
                "ipn:2.1.0",
                "0 8 * * *",
                std::time::Duration::from_secs(3600),
                None,
            )],
            100,
            now,
        );

        // Should have activate at 08:00 and deactivate at 09:00
        assert_eq!(sched.timeline.len(), 2);
        let first = sched.timeline.first().unwrap();
        assert_eq!(first.time, datetime!(2026-03-27 08:00:00 UTC));
        assert_eq!(first.kind, EventKind::Activate);
    }

    #[tokio::test]
    async fn recurring_active_at_startup() {
        let (sink, rx) = MockSink::new();
        let mut sched = make_sched(sink);

        // Now is 08:30 — inside the 08:00-09:00 window
        let now = datetime!(2026-03-27 08:30:00 UTC);

        let result = sched.handle_add(
            "src",
            vec![recurring_contact(
                "ipn:2.*.*",
                "ipn:2.1.0",
                "0 8 * * *",
                std::time::Duration::from_secs(3600),
                None,
            )],
            100,
            now,
        );

        assert_eq!(result.active, 1); // immediately activated
        assert_eq!(sched.timeline.len(), 1); // deactivate at 09:00

        let deactivate = sched.timeline.first().unwrap();
        assert_eq!(deactivate.time, datetime!(2026-03-27 09:00:00 UTC));
        assert_eq!(deactivate.kind, EventKind::Deactivate);

        flush().await;
        let ops = drain_ops(&rx);
        assert_eq!(ops.len(), 1);
        assert!(matches!(ops[0], RouteOp::Add { .. }));
    }

    #[tokio::test]
    async fn recurring_reschedules_after_deactivate() {
        let (sink, _rx) = MockSink::new();
        let mut sched = make_sched(sink);

        let now = datetime!(2026-03-27 07:00:00 UTC);

        sched.handle_add(
            "src",
            vec![recurring_contact(
                "ipn:2.*.*",
                "ipn:2.1.0",
                "0 8 * * *",
                std::time::Duration::from_secs(3600),
                None,
            )],
            100,
            now,
        );

        // Process activate at 08:00
        sched.process_due_events(datetime!(2026-03-27 08:00:00 UTC));
        // Process deactivate at 09:00 — should schedule next day
        sched.process_due_events(datetime!(2026-03-27 09:00:00 UTC));

        // Next occurrence: 2026-03-28 08:00
        assert_eq!(sched.timeline.len(), 2);
        let next = sched.timeline.first().unwrap();
        assert_eq!(next.time, datetime!(2026-03-28 08:00:00 UTC));
        assert_eq!(next.kind, EventKind::Activate);
    }

    #[tokio::test]
    async fn recurring_respects_until() {
        let (sink, _rx) = MockSink::new();
        let mut sched = make_sched(sink);

        let now = datetime!(2026-03-27 07:00:00 UTC);
        let until = datetime!(2026-03-28 00:00:00 UTC);

        sched.handle_add(
            "src",
            vec![recurring_contact(
                "ipn:2.*.*",
                "ipn:2.1.0",
                "0 8 * * *",
                std::time::Duration::from_secs(3600),
                Some(until),
            )],
            100,
            now,
        );

        // Process today's occurrence
        sched.process_due_events(datetime!(2026-03-27 08:00:00 UTC));
        sched.process_due_events(datetime!(2026-03-27 09:00:00 UTC));

        // Next would be 2026-03-28 08:00 but until is 2026-03-28 00:00
        assert!(sched.timeline.is_empty());
    }

    // ── Replace diffing ─────────────────────────────────────────────

    #[tokio::test]
    async fn replace_computes_diff() {
        let (sink, rx) = MockSink::new();
        let mut sched = make_sched(sink);

        let now = datetime!(2026-03-27 08:00:00 UTC);

        // Initial set: A, B
        sched.handle_add(
            "src",
            vec![
                permanent_contact("ipn:2.*.*", "ipn:2.1.0"),
                permanent_contact("ipn:3.*.*", "ipn:3.1.0"),
            ],
            100,
            now,
        );
        flush().await;
        drain_ops(&rx);

        // Replace with B, C — A removed, B unchanged, C added
        let result = sched.handle_replace(
            "src",
            vec![
                permanent_contact("ipn:3.*.*", "ipn:3.1.0"),
                permanent_contact("ipn:4.*.*", "ipn:4.1.0"),
            ],
            100,
            now,
        );

        assert_eq!(result.added, 1);
        assert_eq!(result.removed, 1);
        assert_eq!(result.unchanged, 1);

        flush().await;
        let ops = drain_ops(&rx);
        assert!(ops.contains(&RouteOp::Remove {
            pattern: pat("ipn:2.*.*"),
            action: via("ipn:2.1.0"),
            priority: 100,
        }));
        assert!(ops.contains(&RouteOp::Add {
            pattern: pat("ipn:4.*.*"),
            action: via("ipn:4.1.0"),
            priority: 100,
        }));
    }

    // ── Source withdrawal ───────────────────────────────────────────

    #[tokio::test]
    async fn withdraw_removes_all_contacts() {
        let (sink, rx) = MockSink::new();
        let mut sched = make_sched(sink);

        let now = datetime!(2026-03-27 08:00:00 UTC);

        sched.handle_add(
            "src",
            vec![
                permanent_contact("ipn:2.*.*", "ipn:2.1.0"),
                permanent_contact("ipn:3.*.*", "ipn:3.1.0"),
            ],
            100,
            now,
        );
        flush().await;
        drain_ops(&rx);

        sched.withdraw_source("src");
        flush().await;

        let ops = drain_ops(&rx);
        assert_eq!(ops.len(), 2);
        assert!(ops.iter().all(|op| matches!(op, RouteOp::Remove { .. })));
        assert!(sched.contacts.is_empty());
        assert!(sched.sources.is_empty());
    }

    // ── Source isolation ─────────────────────────────────────────────

    #[tokio::test]
    async fn sources_are_isolated() {
        let (sink, rx) = MockSink::new();
        let mut sched = make_sched(sink);

        let now = datetime!(2026-03-27 08:00:00 UTC);

        sched.handle_add(
            "src_a",
            vec![permanent_contact("ipn:2.*.*", "ipn:2.1.0")],
            100,
            now,
        );
        sched.handle_add(
            "src_b",
            vec![permanent_contact("ipn:3.*.*", "ipn:3.1.0")],
            100,
            now,
        );
        flush().await;
        drain_ops(&rx);

        // Withdraw only src_a
        sched.withdraw_source("src_a");
        flush().await;
        let ops = drain_ops(&rx);

        assert_eq!(ops.len(), 1);
        assert_eq!(
            ops[0],
            RouteOp::Remove {
                pattern: pat("ipn:2.*.*"),
                action: via("ipn:2.1.0"),
                priority: 100,
            }
        );

        // src_b still has its contact
        assert_eq!(sched.contacts.len(), 1);
    }

    // ── Refcounting ─────────────────────────────────────────────────

    #[tokio::test]
    async fn refcount_dedup_same_route() {
        let (sink, rx) = MockSink::new();
        let mut sched = make_sched(sink);

        let now = datetime!(2026-03-27 08:00:00 UTC);

        // Two sources provide the same route
        sched.handle_add(
            "src_a",
            vec![permanent_contact("ipn:2.*.*", "ipn:2.1.0")],
            100,
            now,
        );
        sched.handle_add(
            "src_b",
            vec![permanent_contact("ipn:2.*.*", "ipn:2.1.0")],
            100,
            now,
        );
        flush().await;
        let ops = drain_ops(&rx);

        // Only one add_route call
        assert_eq!(ops.len(), 1);

        // Withdraw src_a — route still held by src_b, no remove
        sched.withdraw_source("src_a");
        flush().await;
        assert!(drain_ops(&rx).is_empty());

        // Withdraw src_b — last holder, remove_route fires
        sched.withdraw_source("src_b");
        flush().await;
        let ops = drain_ops(&rx);
        assert_eq!(ops.len(), 1);
        assert!(matches!(ops[0], RouteOp::Remove { .. }));
    }

    // ── Remove by content ───────────────────────────────────────────

    #[tokio::test]
    async fn remove_matches_by_content() {
        let (sink, rx) = MockSink::new();
        let mut sched = make_sched(sink);

        let now = datetime!(2026-03-27 08:00:00 UTC);

        sched.handle_add(
            "src",
            vec![
                permanent_contact("ipn:2.*.*", "ipn:2.1.0"),
                permanent_contact("ipn:3.*.*", "ipn:3.1.0"),
            ],
            100,
            now,
        );
        flush().await;
        drain_ops(&rx);

        let result = sched.handle_remove("src", vec![permanent_contact("ipn:2.*.*", "ipn:2.1.0")]);

        assert_eq!(result.removed, 1);
        assert_eq!(sched.contacts.len(), 1); // ipn:3 remains

        flush().await;
        let ops = drain_ops(&rx);
        assert_eq!(ops.len(), 1);
        assert!(matches!(ops[0], RouteOp::Remove { .. }));
    }
}
