use super::*;

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
        mut bundle: bundle::Bundle,
        mut data: Bytes,
        key_provider: F,
    ) -> Result<registry::ExecResult, crate::Error>
    where
        F: Fn(&hardy_bpv7::bundle::Bundle, &[u8]) -> Box<dyn hardy_bpv7::bpsec::key::KeySource>
            + Clone
            + Send,
    {
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
                        return Ok(registry::ExecResult::Drop(bundle, reason));
                    }
                }
            }

            for filter in level.writers {
                match filter.filter(&bundle, &data).await? {
                    RewriteResult::Continue(writable, new_data) => {
                        if let Some(writable) = writable {
                            debug!("WriteFilter rewrote bundle metadata");
                            bundle.metadata.writable = writable;
                        }
                        if let Some(new_data) = new_data {
                            debug!("WriteFilter rewrote bundle data");
                            let parsed =
                                hardy_bpv7::bundle::CheckedBundle::parse(&new_data, &key_provider)?;
                            data = Bytes::from(parsed.new_data.unwrap_or(new_data));
                            bundle.bundle = parsed.bundle;
                        }
                    }
                    RewriteResult::Drop(reason) => {
                        debug!("WriteFilter dropped bundle: {reason:?}");
                        return Ok(registry::ExecResult::Drop(bundle, reason));
                    }
                }
            }
        }

        Ok(registry::ExecResult::Continue(
            registry::Mutation::default(),
            bundle,
            data,
        ))
    }
}
