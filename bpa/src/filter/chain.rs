use core::ops::ControlFlow;

use hardy_async::TaskPool;
use hardy_bpv7::bpsec::key::KeySource;
use hardy_bpv7::bundle::{Bundle as Bpv7Bundle, CheckedBundle};
use hardy_bpv7::status_report::ReasonCode;
use trace_err::*;
use tracing::debug;

use super::{
    Error, ExecResult, Filter, Mutation, ReadFilter, ReadResult, WriteFilter, WriteResult,
};

use crate::bundle::Bundle;
use crate::{Arc, Bytes, HashSet};

struct FilterEntry {
    name: String,
    after: HashSet<String>,
}

struct LevelBuilder {
    readers: Vec<(FilterEntry, Arc<dyn ReadFilter>)>,
    writers: Vec<(FilterEntry, Arc<dyn WriteFilter>)>,
}

impl LevelBuilder {
    fn is_empty(&self) -> bool {
        self.readers.is_empty() && self.writers.is_empty()
    }

    fn names(&self) -> impl Iterator<Item = &str> {
        self.readers
            .iter()
            .map(|(e, _)| e.name.as_str())
            .chain(self.writers.iter().map(|(e, _)| e.name.as_str()))
    }

    fn entries(&self) -> impl Iterator<Item = &FilterEntry> {
        self.readers
            .iter()
            .map(|(e, _)| e)
            .chain(self.writers.iter().map(|(e, _)| e))
    }
}

/// Mutable filter registration for a single hook.
///
/// Filters are organized into levels based on dependencies. Filters at the
/// same level have no mutual dependencies: readers run in parallel, writers
/// run sequentially. Call [`build`](Self::build) to produce an immutable
/// [`FilterChain`] for execution.
#[derive(Default)]
pub struct FilterChainBuilder {
    levels: Vec<LevelBuilder>,
}

impl FilterChainBuilder {
    #[cfg(test)]
    pub fn level_count(&self) -> usize {
        self.levels.len()
    }

    #[cfg(test)]
    pub fn names_at_level(&self, level: usize) -> Vec<&str> {
        self.levels
            .get(level)
            .map(|l| l.names().collect())
            .unwrap_or_default()
    }

    pub fn add_filter(&mut self, name: &str, filter: Filter, after: &[&str]) -> Result<(), Error> {
        for level in &self.levels {
            if level.names().any(|n| n == name) {
                return Err(Error::AlreadyExists(name.into()));
            }
        }

        // Insert after the last level containing a dependency
        let mut insert_at = 0;
        let mut unresolved: HashSet<&str> = after.iter().copied().collect();

        for (i, level) in self.levels.iter().enumerate() {
            let mut found_dep = false;
            for n in level.names() {
                if unresolved.remove(n) {
                    found_dep = true;
                }
            }
            if found_dep {
                insert_at = i + 1;
            }
        }

        if !unresolved.is_empty() {
            return Err(Error::DependencyNotFound(
                unresolved.into_iter().collect::<Vec<_>>().join(", "),
            ));
        }

        let entry = FilterEntry {
            name: name.into(),
            after: after.iter().map(|s| s.to_string()).collect(),
        };

        if insert_at >= self.levels.len() {
            self.levels.push(LevelBuilder {
                readers: Vec::new(),
                writers: Vec::new(),
            });
        }

        match filter {
            Filter::Read(f) => self.levels[insert_at].readers.push((entry, f)),
            Filter::Write(f) => self.levels[insert_at].writers.push((entry, f)),
        }

        Ok(())
    }

    pub fn remove_filter(&mut self, name: &str) -> Result<Option<Filter>, Error> {
        let dependants: Vec<String> = self
            .levels
            .iter()
            .flat_map(|level| level.entries())
            .filter(|e| e.after.contains(name))
            .map(|e| e.name.clone())
            .collect();

        if !dependants.is_empty() {
            return Err(Error::HasDependants(name.to_string(), dependants));
        }

        let mut removed = None;
        for level in &mut self.levels {
            if let Some(idx) = level.readers.iter().position(|(e, _)| e.name == name) {
                let (_, filter) = level.readers.remove(idx);
                removed = Some(Filter::Read(filter));
                break;
            }
            if let Some(idx) = level.writers.iter().position(|(e, _)| e.name == name) {
                let (_, filter) = level.writers.remove(idx);
                removed = Some(Filter::Write(filter));
                break;
            }
        }

        if removed.is_some() {
            self.levels.retain(|l| !l.is_empty());
        }

        Ok(removed)
    }

    pub fn build(&self) -> FilterChain {
        FilterChain {
            levels: self
                .levels
                .iter()
                .map(|level| Level {
                    readers: level.readers.iter().map(|(_, f)| f.clone()).collect(),
                    writers: level.writers.iter().map(|(_, f)| f.clone()).collect(),
                })
                .collect(),
        }
    }
}

/// Execution level: readers run in parallel, then writers run sequentially.
struct Level {
    readers: Vec<Arc<dyn ReadFilter>>,
    writers: Vec<Arc<dyn WriteFilter>>,
}

impl Level {
    /// Run all readers in parallel on `pool`. Takes ownership to avoid
    /// cloning, returns the bundle and data back via `Arc::try_unwrap`.
    ///
    /// `pool` must be the filter engine's dedicated unbounded pool, never
    /// one whose permits exec() callers may already hold (such as the
    /// dispatcher's processing pool): permit-holding tasks parked waiting
    /// for further permits from the same semaphore deadlock the whole pool
    /// once it saturates.
    async fn run_readers(
        &self,
        pool: &TaskPool,
        bundle: Bundle,
        data: Bytes,
    ) -> Result<(Bundle, Bytes, ControlFlow<Option<ReasonCode>>), crate::Error> {
        if self.readers.is_empty() {
            return Ok((bundle, data, ControlFlow::Continue(())));
        }

        // Fast path: single reader runs inline, no task spawn overhead
        if self.readers.len() == 1 {
            let result = self.readers[0].filter(&bundle, data.as_ref()).await?;
            if let ReadResult::Drop(reason) = result {
                debug!("ReadFilter dropped bundle: {reason:?}");
                return Ok((bundle, data, ControlFlow::Break(reason)));
            }
            return Ok((bundle, data, ControlFlow::Continue(())));
        }

        // Multiple readers: spawn in parallel
        let shared = Arc::new((bundle, data));

        let mut handles = Vec::new();
        for filter in &self.readers {
            let shared = shared.clone();
            let filter = filter.clone();
            handles.push(hardy_async::spawn!(pool, "filter_task", async move {
                let (bundle, data) = &*shared;
                filter.filter(bundle, data.as_ref()).await
            }));
        }

        let mut results = Vec::new();
        for handle in handles {
            results.push(handle.await.trace_expect("filter spawn failed!")?);
        }

        let (bundle, data) = Arc::try_unwrap(shared).trace_expect("Lingering filter tasks?!?");

        for result in results {
            if let ReadResult::Drop(reason) = result {
                debug!("ReadFilter dropped bundle: {reason:?}");
                return Ok((bundle, data, ControlFlow::Break(reason)));
            }
        }

        Ok((bundle, data, ControlFlow::Continue(())))
    }

    /// Run all writers sequentially.
    async fn run_writers<F>(
        &self,
        bundle: &mut Bundle,
        data: &mut Bytes,
        mutation: &mut Mutation,
        key_provider: &F,
    ) -> Result<ControlFlow<Option<ReasonCode>>, crate::Error>
    where
        F: Fn(&Bpv7Bundle, &[u8]) -> Box<dyn KeySource>,
    {
        for filter in &self.writers {
            match filter.filter(bundle, data).await? {
                WriteResult::Continue(writable, new_data) => {
                    if let Some(writable) = writable {
                        debug!("WriteFilter rewrote bundle metadata");
                        mutation.metadata = true;
                        bundle.metadata.writable = writable;
                    }
                    if let Some(mut new_data) = new_data {
                        debug!("WriteFilter rewrote bundle data");
                        mutation.data = true;
                        let parsed = CheckedBundle::parse(&new_data, key_provider)?;
                        if let Some(chunks) = parsed.new_data {
                            hardy_bpv7::editor::Chunk::flatten_inplace(chunks, &mut new_data);
                        }
                        *data = Bytes::from(new_data);
                        bundle.bundle = parsed.bundle;
                    }
                }
                WriteResult::Drop(reason) => {
                    debug!("WriteFilter dropped bundle: {reason:?}");
                    return Ok(ControlFlow::Break(reason));
                }
            }
        }

        Ok(ControlFlow::Continue(()))
    }
}

/// Immutable filter chain, ready to execute.
///
/// Built from a [`FilterChainBuilder`] and cached for repeated execution.
#[derive(Default)]
pub struct FilterChain {
    levels: Vec<Level>,
}

impl FilterChain {
    pub async fn exec<F>(
        &self,
        pool: &TaskPool,
        mut bundle: Bundle,
        mut data: Bytes,
        key_provider: F,
    ) -> Result<ExecResult, crate::Error>
    where
        F: Fn(&Bpv7Bundle, &[u8]) -> Box<dyn KeySource>,
    {
        let mut mutation = Mutation::default();

        for level in &self.levels {
            match level.run_readers(pool, bundle, data).await? {
                (b, d, ControlFlow::Continue(())) => (bundle, data) = (b, d),
                (b, _, ControlFlow::Break(reason)) => return Ok(ExecResult::Drop(b, reason)),
            }
            if let ControlFlow::Break(reason) = level
                .run_writers(&mut bundle, &mut data, &mut mutation, &key_provider)
                .await?
            {
                return Ok(ExecResult::Drop(bundle, reason));
            }
        }

        Ok(ExecResult::Continue(mutation, bundle, data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hardy_async::async_trait;
    use hardy_bpv7::status_report::ReasonCode;

    struct PassFilter;

    #[async_trait]
    impl ReadFilter for PassFilter {
        async fn filter(&self, _bundle: &Bundle, _data: &[u8]) -> Result<ReadResult, crate::Error> {
            Ok(ReadResult::Continue)
        }
    }

    struct DropFilter;

    #[async_trait]
    impl ReadFilter for DropFilter {
        async fn filter(&self, _bundle: &Bundle, _data: &[u8]) -> Result<ReadResult, crate::Error> {
            Ok(ReadResult::Drop(Some(ReasonCode::NoAdditionalInformation)))
        }
    }

    struct NoopWriter;

    #[async_trait]
    impl WriteFilter for NoopWriter {
        async fn filter(
            &self,
            _bundle: &Bundle,
            _data: &[u8],
        ) -> Result<WriteResult, crate::Error> {
            Ok(WriteResult::Continue(None, None))
        }
    }

    fn read(name: &str, after: &[&str], chain: &mut FilterChainBuilder) {
        chain
            .add_filter(name, Filter::Read(Arc::new(PassFilter)), after)
            .unwrap();
    }

    fn write(name: &str, after: &[&str], chain: &mut FilterChainBuilder) {
        chain
            .add_filter(name, Filter::Write(Arc::new(NoopWriter)), after)
            .unwrap();
    }

    // --- Registration ---

    #[test]
    fn add_no_deps() {
        let mut chain = FilterChainBuilder::default();
        read("a", &[], &mut chain);
        read("b", &[], &mut chain);
        write("c", &[], &mut chain);

        assert_eq!(chain.level_count(), 1);
        let names: Vec<&str> = chain.names_at_level(0);
        assert!(names.contains(&"a"));
        assert!(names.contains(&"b"));
        assert!(names.contains(&"c"));
    }

    #[test]
    fn add_linear_deps() {
        let mut chain = FilterChainBuilder::default();
        read("a", &[], &mut chain);
        write("b", &["a"], &mut chain);
        read("c", &["b"], &mut chain);

        assert_eq!(chain.level_count(), 3);
        assert_eq!(chain.names_at_level(0), vec!["a"]);
        assert_eq!(chain.names_at_level(1), vec!["b"]);
        assert_eq!(chain.names_at_level(2), vec!["c"]);
    }

    #[test]
    fn add_parallel_at_same_level() {
        let mut chain = FilterChainBuilder::default();
        write("root", &[], &mut chain);
        read("a", &["root"], &mut chain);
        read("b", &["root"], &mut chain);
        write("c", &["root"], &mut chain);

        assert_eq!(chain.level_count(), 2);
        assert_eq!(chain.names_at_level(0), vec!["root"]);
        let level1 = chain.names_at_level(1);
        assert!(level1.contains(&"a"));
        assert!(level1.contains(&"b"));
        assert!(level1.contains(&"c"));
    }

    #[test]
    fn add_multiple_deps() {
        let mut chain = FilterChainBuilder::default();
        read("a", &[], &mut chain);
        read("b", &[], &mut chain);
        write("c", &["a", "b"], &mut chain);

        assert_eq!(chain.level_count(), 2);
        assert_eq!(chain.names_at_level(1), vec!["c"]);
    }

    #[test]
    fn add_deps_across_non_adjacent_levels() {
        let mut chain = FilterChainBuilder::default();
        read("a", &[], &mut chain);
        read("b", &["a"], &mut chain);
        read("c", &["b"], &mut chain);
        // depends on level 0 and level 2 — should land at level 3
        read("d", &["a", "c"], &mut chain);

        assert_eq!(chain.level_count(), 4);
        assert_eq!(chain.names_at_level(3), vec!["d"]);
    }

    #[test]
    fn add_duplicate_name_errors() {
        let mut chain = FilterChainBuilder::default();
        read("a", &[], &mut chain);

        let err = chain
            .add_filter("a", Filter::Read(Arc::new(PassFilter)), &[])
            .unwrap_err();
        assert!(matches!(err, Error::AlreadyExists(_)));
    }

    #[test]
    fn add_missing_dep_errors() {
        let mut chain = FilterChainBuilder::default();

        let err = chain
            .add_filter("a", Filter::Read(Arc::new(PassFilter)), &["missing"])
            .unwrap_err();
        assert!(matches!(err, Error::DependencyNotFound(_)));
    }

    // --- Removal ---

    #[test]
    fn remove_filter() {
        let mut chain = FilterChainBuilder::default();
        read("a", &[], &mut chain);
        write("b", &[], &mut chain);

        let removed = chain.remove_filter("a").unwrap();
        assert!(removed.is_some());
        assert!(matches!(removed.unwrap(), Filter::Read(_)));
        assert_eq!(chain.names_at_level(0), vec!["b"]);
    }

    #[test]
    fn remove_not_found() {
        let mut chain = FilterChainBuilder::default();
        let removed = chain.remove_filter("x").unwrap();
        assert!(removed.is_none());
    }

    #[test]
    fn remove_with_dependants_errors() {
        let mut chain = FilterChainBuilder::default();
        read("a", &[], &mut chain);
        read("b", &["a"], &mut chain);

        assert!(matches!(
            chain.remove_filter("a"),
            Err(Error::HasDependants(_, _))
        ));
    }

    #[test]
    fn remove_cleans_empty_levels() {
        let mut chain = FilterChainBuilder::default();
        read("a", &[], &mut chain);
        read("b", &["a"], &mut chain);

        // Remove b (level 1), then a (level 0)
        chain.remove_filter("b").unwrap();
        assert_eq!(chain.level_count(), 1);

        chain.remove_filter("a").unwrap();
        assert_eq!(chain.level_count(), 0);
    }

    // --- Clear ---

    #[test]
    fn clear_empties_chain() {
        let mut chain = FilterChainBuilder::default();
        read("a", &[], &mut chain);
        write("b", &["a"], &mut chain);

        chain.levels.clear();
        assert_eq!(chain.level_count(), 0);
    }

    // --- Build ---

    #[test]
    fn build_empty_chain() {
        let builder = FilterChainBuilder::default();
        let chain = builder.build();
        assert!(chain.levels.is_empty());
    }

    // --- Exec ---

    async fn run_chain(builder: &FilterChainBuilder) -> ExecResult {
        let chain = builder.build();
        let pool = hardy_async::TaskPool::new();
        let bundle = Bundle {
            bundle: Default::default(),
            metadata: Default::default(),
        };
        chain
            .exec(&pool, bundle, Bytes::new(), hardy_bpv7::bpsec::no_keys)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn exec_all_continue() {
        let mut chain = FilterChainBuilder::default();
        read("a", &[], &mut chain);
        read("b", &[], &mut chain);

        assert!(matches!(
            run_chain(&chain).await,
            ExecResult::Continue(_, _, _)
        ));
    }

    #[tokio::test]
    async fn exec_read_filter_drops() {
        let mut chain = FilterChainBuilder::default();
        chain
            .add_filter("pass", Filter::Read(Arc::new(PassFilter)), &[])
            .unwrap();
        chain
            .add_filter("drop", Filter::Read(Arc::new(DropFilter)), &[])
            .unwrap();

        assert!(matches!(run_chain(&chain).await, ExecResult::Drop(_, _)));
    }

    #[tokio::test]
    async fn exec_writer_noop() {
        let mut chain = FilterChainBuilder::default();
        write("w", &[], &mut chain);

        assert!(matches!(
            run_chain(&chain).await,
            ExecResult::Continue(_, _, _)
        ));
    }

    #[tokio::test]
    async fn exec_empty_chain() {
        let chain = FilterChainBuilder::default();
        assert!(matches!(
            run_chain(&chain).await,
            ExecResult::Continue(_, _, _)
        ));
    }

    struct RecordingFilter {
        name: &'static str,
        log: Arc<std::sync::Mutex<Vec<&'static str>>>,
    }

    #[async_trait]
    impl ReadFilter for RecordingFilter {
        async fn filter(&self, _bundle: &Bundle, _data: &[u8]) -> Result<ReadResult, crate::Error> {
            self.log.lock().unwrap().push(self.name);
            Ok(ReadResult::Continue)
        }
    }

    struct MetadataWriter;

    #[async_trait]
    impl WriteFilter for MetadataWriter {
        async fn filter(
            &self,
            _bundle: &Bundle,
            _data: &[u8],
        ) -> Result<WriteResult, crate::Error> {
            Ok(WriteResult::Continue(
                Some(crate::bundle::WritableMetadata {
                    flow_label: Some(7),
                }),
                None,
            ))
        }
    }

    struct DropWriter;

    #[async_trait]
    impl WriteFilter for DropWriter {
        async fn filter(
            &self,
            _bundle: &Bundle,
            _data: &[u8],
        ) -> Result<WriteResult, crate::Error> {
            Ok(WriteResult::Drop(Some(ReasonCode::BlockUnintelligible)))
        }
    }

    // Dependency chains place filters at successive levels, and exec must
    // run the levels front to back: a reversed or shuffled level walk
    // produces a different recorded order.
    #[tokio::test]
    async fn exec_runs_levels_in_dependency_order() {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut chain = FilterChainBuilder::default();
        for (name, after) in [("a", [].as_slice()), ("b", &["a"]), ("c", &["b"])] {
            chain
                .add_filter(
                    name,
                    Filter::Read(Arc::new(RecordingFilter {
                        name,
                        log: log.clone(),
                    })),
                    after,
                )
                .unwrap();
        }
        assert_eq!(chain.level_count(), 3);

        assert!(matches!(
            run_chain(&chain).await,
            ExecResult::Continue(_, _, _)
        ));
        assert_eq!(*log.lock().unwrap(), ["a", "b", "c"]);
    }

    #[tokio::test]
    async fn exec_applies_writer_metadata() {
        let mut chain = FilterChainBuilder::default();
        chain
            .add_filter("meta", Filter::Write(Arc::new(MetadataWriter)), &[])
            .unwrap();

        match run_chain(&chain).await {
            ExecResult::Continue(mutation, bundle, _) => {
                assert!(mutation.metadata, "metadata rewrite must be flagged");
                assert!(!mutation.data);
                assert_eq!(bundle.metadata.writable.flow_label, Some(7));
            }
            ExecResult::Drop(_, reason) => panic!("unexpected drop: {reason:?}"),
        }
    }

    #[tokio::test]
    async fn exec_write_filter_drops_with_reason() {
        let mut chain = FilterChainBuilder::default();
        read("pass", &[], &mut chain);
        chain
            .add_filter("veto", Filter::Write(Arc::new(DropWriter)), &[])
            .unwrap();

        assert!(matches!(
            run_chain(&chain).await,
            ExecResult::Drop(_, Some(ReasonCode::BlockUnintelligible))
        ));
    }
}
