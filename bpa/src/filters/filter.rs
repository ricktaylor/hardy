use hardy_bpv7::bpsec::key::KeySource;
use hardy_bpv7::bundle::{Bundle as Bpv7Bundle, CheckedBundle};
use trace_err::*;
use tracing::debug;

use super::registry::{ExecResult, Mutation};
use super::{Error, Filter, FilterResult, ReadFilter, RewriteResult, WriteFilter};
use crate::bundle::Bundle;
use crate::{Arc, Bytes, HashSet};

struct FilterEntry {
    name: String,
    after: HashSet<String>,
}

// Filters at the same level have no mutual dependencies.
// Readers run in parallel, writers run sequentially.
struct Level {
    readers: Vec<(FilterEntry, Arc<dyn ReadFilter>)>,
    writers: Vec<(FilterEntry, Arc<dyn WriteFilter>)>,
}

impl Level {
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

/// The filter DAG for a single hook, stored as a flat list of levels.
#[derive(Default)]
pub struct FilterChain {
    levels: Vec<Level>,
}

impl FilterChain {
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

    pub fn clear(&mut self) {
        self.levels.clear();
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
            self.levels.push(Level {
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

    /// Clone Arc references for lock-free async execution.
    pub fn prepare(&self) -> PreparedFilters {
        PreparedFilters {
            levels: self
                .levels
                .iter()
                .filter(|level| !level.is_empty())
                .map(|level| PreparedLevel {
                    readers: level.readers.iter().map(|(_, f)| f.clone()).collect(),
                    writers: level.writers.iter().map(|(_, f)| f.clone()).collect(),
                })
                .collect(),
        }
    }
}

struct PreparedLevel {
    readers: Vec<Arc<dyn ReadFilter>>,
    writers: Vec<Arc<dyn WriteFilter>>,
}

pub struct PreparedFilters {
    levels: Vec<PreparedLevel>,
}

impl PreparedFilters {
    pub async fn exec<F>(
        self,
        pool: &hardy_async::BoundedTaskPool,
        mut bundle: Bundle,
        mut data: Bytes,
        key_provider: F,
    ) -> Result<ExecResult, crate::Error>
    where
        F: Fn(&Bpv7Bundle, &[u8]) -> Box<dyn KeySource> + Clone + Send,
    {
        let mut mutation = Mutation::default();

        for level in self.levels {
            if !level.readers.is_empty() {
                let bd = Arc::new((bundle, data));

                let mut handles = Vec::new();
                for filter in level.readers {
                    let bd = bd.clone();
                    handles.push(
                        hardy_async::spawn!(pool, "filter_task", async move {
                            let (bundle, data) = &*bd;
                            filter.filter(bundle, data.as_ref()).await
                        })
                        .await,
                    );
                }

                // Await all tasks so we can recover the original bundle from the Arc
                let mut results = Vec::new();
                for handle in handles {
                    results.push(handle.await.trace_expect("filter spawn failed!")?);
                }

                (bundle, data) = Arc::try_unwrap(bd).trace_expect("Lingering filter tasks?!?");

                for result in results {
                    if let FilterResult::Drop(reason) = result {
                        debug!("ReadFilter dropped bundle: {reason:?}");
                        return Ok(ExecResult::Drop(bundle, reason));
                    }
                }
            }

            for filter in level.writers {
                match filter.filter(&bundle, &data).await? {
                    RewriteResult::Continue(writable, new_data) => {
                        if let Some(writable) = writable {
                            debug!("WriteFilter rewrote bundle metadata");
                            mutation.metadata = true;
                            bundle.metadata.writable = writable;
                        }
                        if let Some(new_data) = new_data {
                            debug!("WriteFilter rewrote bundle data");
                            mutation.data = true;
                            let parsed = CheckedBundle::parse(&new_data, &key_provider)?;
                            data = Bytes::from(parsed.new_data.unwrap_or(new_data));
                            bundle.bundle = parsed.bundle;
                        }
                    }
                    RewriteResult::Drop(reason) => {
                        debug!("WriteFilter dropped bundle: {reason:?}");
                        return Ok(ExecResult::Drop(bundle, reason));
                    }
                }
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
        async fn filter(
            &self,
            _bundle: &Bundle,
            _data: &[u8],
        ) -> Result<FilterResult, crate::Error> {
            Ok(FilterResult::Continue)
        }
    }

    struct DropFilter;

    #[async_trait]
    impl ReadFilter for DropFilter {
        async fn filter(
            &self,
            _bundle: &Bundle,
            _data: &[u8],
        ) -> Result<FilterResult, crate::Error> {
            Ok(FilterResult::Drop(ReasonCode::NoAdditionalInformation))
        }
    }

    struct NoopWriter;

    #[async_trait]
    impl WriteFilter for NoopWriter {
        async fn filter(
            &self,
            _bundle: &Bundle,
            _data: &[u8],
        ) -> Result<RewriteResult, crate::Error> {
            Ok(RewriteResult::Continue(None, None))
        }
    }

    fn read(name: &str, after: &[&str], chain: &mut FilterChain) {
        chain
            .add_filter(name, Filter::Read(Arc::new(PassFilter)), after)
            .unwrap();
    }

    fn write(name: &str, after: &[&str], chain: &mut FilterChain) {
        chain
            .add_filter(name, Filter::Write(Arc::new(NoopWriter)), after)
            .unwrap();
    }

    // --- Registration ---

    #[test]
    fn add_no_deps() {
        let mut chain = FilterChain::default();
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
        let mut chain = FilterChain::default();
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
        let mut chain = FilterChain::default();
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
        let mut chain = FilterChain::default();
        read("a", &[], &mut chain);
        read("b", &[], &mut chain);
        write("c", &["a", "b"], &mut chain);

        assert_eq!(chain.level_count(), 2);
        assert_eq!(chain.names_at_level(1), vec!["c"]);
    }

    #[test]
    fn add_deps_across_non_adjacent_levels() {
        let mut chain = FilterChain::default();
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
        let mut chain = FilterChain::default();
        read("a", &[], &mut chain);

        let err = chain
            .add_filter("a", Filter::Read(Arc::new(PassFilter)), &[])
            .unwrap_err();
        assert!(matches!(err, Error::AlreadyExists(_)));
    }

    #[test]
    fn add_missing_dep_errors() {
        let mut chain = FilterChain::default();

        let err = chain
            .add_filter("a", Filter::Read(Arc::new(PassFilter)), &["missing"])
            .unwrap_err();
        assert!(matches!(err, Error::DependencyNotFound(_)));
    }

    // --- Removal ---

    #[test]
    fn remove_filter() {
        let mut chain = FilterChain::default();
        read("a", &[], &mut chain);
        write("b", &[], &mut chain);

        let removed = chain.remove_filter("a").unwrap();
        assert!(removed.is_some());
        assert!(matches!(removed.unwrap(), Filter::Read(_)));
        assert_eq!(chain.names_at_level(0), vec!["b"]);
    }

    #[test]
    fn remove_not_found() {
        let mut chain = FilterChain::default();
        let removed = chain.remove_filter("x").unwrap();
        assert!(removed.is_none());
    }

    #[test]
    fn remove_with_dependants_errors() {
        let mut chain = FilterChain::default();
        read("a", &[], &mut chain);
        read("b", &["a"], &mut chain);

        assert!(matches!(
            chain.remove_filter("a"),
            Err(Error::HasDependants(_, _))
        ));
    }

    #[test]
    fn remove_cleans_empty_levels() {
        let mut chain = FilterChain::default();
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
        let mut chain = FilterChain::default();
        read("a", &[], &mut chain);
        write("b", &["a"], &mut chain);

        chain.clear();
        assert_eq!(chain.level_count(), 0);
    }

    // --- Prepare ---

    #[test]
    fn prepare_empty_chain() {
        let chain = FilterChain::default();
        let prepared = chain.prepare();
        assert_eq!(prepared.levels.len(), 0);
    }

    // --- Exec ---

    async fn run_chain(chain: &FilterChain) -> ExecResult {
        let prepared = chain.prepare();
        let pool = hardy_async::BoundedTaskPool::new(core::num::NonZeroUsize::new(4).unwrap());
        let bundle = Bundle {
            bundle: Default::default(),
            metadata: Default::default(),
        };
        prepared
            .exec(&pool, bundle, Bytes::new(), hardy_bpv7::bpsec::no_keys)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn exec_all_continue() {
        let mut chain = FilterChain::default();
        read("a", &[], &mut chain);
        read("b", &[], &mut chain);

        assert!(matches!(
            run_chain(&chain).await,
            ExecResult::Continue(_, _, _)
        ));
    }

    #[tokio::test]
    async fn exec_read_filter_drops() {
        let mut chain = FilterChain::default();
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
        let mut chain = FilterChain::default();
        write("w", &[], &mut chain);

        assert!(matches!(
            run_chain(&chain).await,
            ExecResult::Continue(_, _, _)
        ));
    }

    #[tokio::test]
    async fn exec_empty_chain() {
        let chain = FilterChain::default();
        assert!(matches!(
            run_chain(&chain).await,
            ExecResult::Continue(_, _, _)
        ));
    }
}
