use super::*;

// A single node's worth of filters, ready for execution (just Arc clones)
struct PreparedNode {
    read_only: Vec<Arc<dyn ReadFilter>>,
    read_write: Vec<Arc<dyn WriteFilter>>,
}

// A prepared filter chain ready for execution.
// Obtained by calling FilterNode::prepare() while holding a lock, then executed
// after releasing the lock. Only contains Arc clones (cheap refcount bumps).
pub struct PreparedFilters {
    nodes: Vec<PreparedNode>,
}

// A named filter with its dependency information.
struct FilterItem<T> {
    name: String,
    filter: T,
    after: HashSet<String>, // Names of filters that this filter must run after
}

// A node in the filter execution graph.
//
// Filters are organized as a linked list of nodes, where each node contains
// filters that can execute at the same "level" (no dependencies on each other).
// Read-only filters within a node execute in parallel; read-write filters
// execute sequentially. The `next` pointer chains to filters that depend on
// filters in this node.
pub struct FilterNode {
    read_only: Vec<FilterItem<Arc<dyn ReadFilter>>>, // Run in parallel
    read_write: Vec<FilterItem<Arc<dyn WriteFilter>>>, // Run sequentially
    next: Option<Box<FilterNode>>,                   // Filters that depend on this node's filters
}

impl Default for FilterNode {
    fn default() -> Self {
        Self::new()
    }
}

impl FilterNode {
    // Creates an empty filter node
    pub fn new() -> Self {
        Self {
            read_only: Vec::new(),
            read_write: Vec::new(),
            next: None,
        }
    }

    pub fn clear(&mut self) {
        if let Some(mut next) = self.next.take() {
            next.clear();
        }
        self.read_only.clear();
        self.read_write.clear();
    }

    // Registers a filter with the given name and dependencies.
    // The filter is placed in the node chain based on its `after` dependencies.
    // Errors if a filter with this name already exists or any dependency is not found.
    pub fn add_filter(&mut self, name: &str, filter: Filter, after: &[&str]) -> Result<(), Error> {
        let mut tracking_after = after.iter().copied().collect::<HashSet<&str>>();
        self.add_filter_inner(
            name,
            filter,
            &mut tracking_after,
            after.iter().map(|s| s.to_string()).collect(),
        )
    }

    // Recursive helper for add_filter. Walks the node chain, removing found
    // dependencies from `tracking_after`. Once all dependencies are resolved,
    // inserts the filter at the current node.
    fn add_filter_inner(
        &mut self,
        name: &str,
        filter: Filter,
        tracking_after: &mut HashSet<&str>,
        after: HashSet<String>,
    ) -> Result<(), Error> {
        if self.find_dependencies(name, tracking_after)? {
            self.next
                .get_or_insert_with(|| Box::new(FilterNode::new()))
                .add_filter_inner(name, filter, tracking_after, after)
        } else if tracking_after.is_empty() {
            if let Some(next) = &self.next {
                next.check_names(name)?;
            }
            match filter {
                Filter::Read(f) => self.read_only.push(FilterItem {
                    name: name.into(),
                    filter: f,
                    after,
                }),
                Filter::Write(f) => self.read_write.push(FilterItem {
                    name: name.into(),
                    filter: f,
                    after,
                }),
            }
            Ok(())
        } else {
            // Reached end without finding dependency
            Err(Error::DependencyNotFound(format!("{:?}", tracking_after)))
        }
    }

    // Scans this node for dependencies and duplicate names.
    // Returns Ok(true) if any dependency was found (must continue to next node),
    // Ok(false) if none found, or Err(AlreadyExists) if name is a duplicate.
    fn find_dependencies(&self, name: &str, after: &mut HashSet<&str>) -> Result<bool, Error> {
        let mut next = false;
        for n in self
            .read_only
            .iter()
            .map(|item| &item.name)
            .chain(self.read_write.iter().map(|item| &item.name))
        {
            if n == name {
                return Err(Error::AlreadyExists(n.clone()));
            }
            if after.remove(n.as_str()) {
                next = true;
            }
        }
        Ok(next)
    }

    // Recursively checks that no filter with `name` exists in this node or beyond.
    fn check_names(&self, name: &str) -> Result<(), Error> {
        if self
            .read_only
            .iter()
            .map(|item| &item.name)
            .chain(self.read_write.iter().map(|item| &item.name))
            .any(|n| n == name)
        {
            return Err(Error::AlreadyExists(name.into()));
        }
        if let Some(next) = &self.next {
            next.check_names(name)?;
        }
        Ok(())
    }

    // Remove a filter by name.
    // Returns Ok(Some(filter)) if removed, Ok(None) if not found,
    // or Err(HasDependants) if other filters depend on it.
    pub fn remove_filter(&mut self, name: &str) -> Result<Option<Filter>, Error> {
        // First, check for dependants across the entire chain
        let dependants = self.find_dependants(name);
        if !dependants.is_empty() {
            return Err(Error::HasDependants(name.to_string(), dependants));
        }

        // Now remove the filter
        Ok(self.remove_filter_inner(name))
    }

    // Collect names of all filters that depend on the given name
    fn find_dependants(&self, name: &str) -> Vec<String> {
        let mut dependants = Vec::new();
        self.find_dependants_inner(name, &mut dependants);
        dependants
    }

    // Recursive helper that accumulates dependant names into the provided Vec
    fn find_dependants_inner(&self, name: &str, dependants: &mut Vec<String>) {
        for (filter_name, after) in self
            .read_only
            .iter()
            .map(|item| (&item.name, &item.after))
            .chain(self.read_write.iter().map(|item| (&item.name, &item.after)))
        {
            if after.contains(name) {
                dependants.push(filter_name.clone());
            }
        }
        if let Some(next) = &self.next {
            next.find_dependants_inner(name, dependants);
        }
    }

    // Remove filter by name, returning it if found. Also cleans up empty nodes.
    fn remove_filter_inner(&mut self, name: &str) -> Option<Filter> {
        // Try read_only
        if let Some(idx) = self.read_only.iter().position(|f| f.name == name) {
            return Some(Filter::Read(self.read_only.remove(idx).filter));
        }

        // Try read_write
        if let Some(idx) = self.read_write.iter().position(|f| f.name == name) {
            return Some(Filter::Write(self.read_write.remove(idx).filter));
        }

        // Try next node
        if let Some(next) = &mut self.next {
            let result = next.remove_filter_inner(name);

            // Clean up empty intermediate nodes
            if next.read_only.is_empty() && next.read_write.is_empty() {
                self.next = next.next.take();
            }

            return result;
        }

        None
    }

    // Prepares the filter chain for execution by cloning all Arc references.
    // Call this while holding a lock, then release the lock before calling exec().
    pub fn prepare(&self) -> PreparedFilters {
        let mut nodes = Vec::new();
        self.prepare_inner(&mut nodes);
        PreparedFilters { nodes }
    }

    fn prepare_inner(&self, nodes: &mut Vec<PreparedNode>) {
        nodes.push(PreparedNode {
            read_only: self
                .read_only
                .iter()
                .map(|item| item.filter.clone())
                .collect(),
            read_write: self
                .read_write
                .iter()
                .map(|item| item.filter.clone())
                .collect(),
        });
        if let Some(next) = &self.next {
            next.prepare_inner(nodes);
        }
    }
}

impl PreparedFilters {
    // Execute the prepared filter chain on a bundle.
    // Read-only filters run in parallel, then read-write filters run sequentially.
    // Returns Drop on first filter rejection.
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
        // Capture what has changed
        let mut mutation = registry::Mutation::default();

        for node in self.nodes {
            if !node.read_only.is_empty() {
                // Wrap bundle/data in Arc for parallel access
                let bd = Arc::new((bundle, data));

                // Execute read-only filters in parallel
                let mut read_results = Vec::new();
                for filter in node.read_only {
                    let bd = bd.clone();
                    read_results.push(
                        hardy_async::spawn!(pool, "filter_task", async move {
                            let (bundle, data) = &*bd;
                            filter.filter(bundle, data.as_ref()).await
                        })
                        .await,
                    );
                }

                // Check results - this is a 'barrier'
                for result in read_results {
                    if let FilterResult::Drop(reason) =
                        result.await.trace_expect("filter spawn failed!")?
                    {
                        debug!("ReadFilter dropped bundle: {reason:?}");

                        // Create a drop_bundle with just enough of the Bundle that we can reply with something suitable for dispatcher::drop_bundle() if needed.  See report_bundle_deletion() for details.
                        let drop_bundle = bundle::Bundle {
                            bundle: hardy_bpv7::bundle::Bundle {
                                id: bd.0.bundle.id.clone(),
                                flags: bd.0.bundle.flags.clone(),
                                report_to: bd.0.bundle.report_to.clone(),
                                ..Default::default()
                            },
                            metadata: metadata::BundleMetadata {
                                storage_name: bd.0.metadata.storage_name.clone(),
                                ..Default::default()
                            },
                        };
                        return Ok(registry::ExecResult::Drop(drop_bundle, reason));
                    }
                }

                // All tasks completed, unwrap the Arc
                (bundle, data) = Arc::try_unwrap(bd).trace_expect("Lingering filter tasks?!?");
            }

            // Execute read-write filters sequentially
            for filter in node.read_write {
                (bundle, data) = match filter.filter(&bundle, &data).await? {
                    RewriteResult::Continue(None, None) => (bundle, data),
                    RewriteResult::Continue(Some(writable), None) => {
                        debug!("WriteFilter rewrote bundle metadata");

                        mutation.metadata = true;

                        (
                            bundle::Bundle {
                                bundle: bundle.bundle,
                                metadata: metadata::BundleMetadata {
                                    storage_name: bundle.metadata.storage_name,
                                    status: bundle.metadata.status,
                                    read_only: bundle.metadata.read_only,
                                    writable,
                                },
                            },
                            data,
                        )
                    }
                    RewriteResult::Continue(metadata, Some(new_data)) => {
                        let metadata = if let Some(writable) = metadata {
                            debug!("WriteFilter rewrote bundle data and metadata");
                            mutation.metadata = true;
                            metadata::BundleMetadata {
                                storage_name: bundle.metadata.storage_name,
                                status: bundle.metadata.status,
                                read_only: bundle.metadata.read_only,
                                writable,
                            }
                        } else {
                            debug!("WriteFilter rewrote bundle data");
                            bundle.metadata
                        };

                        mutation.bundle = true;

                        let parsed =
                            hardy_bpv7::bundle::CheckedBundle::parse(&new_data, &key_provider)?;
                        let data = Bytes::from(parsed.new_data.unwrap_or(new_data));
                        (
                            bundle::Bundle {
                                bundle: parsed.bundle,
                                metadata,
                            },
                            data,
                        )
                    }
                    RewriteResult::Drop(reason) => {
                        debug!("WriteFilter dropped bundle: {reason:?}");
                        return Ok(registry::ExecResult::Drop(bundle, reason));
                    }
                };
            }
        }

        Ok(registry::ExecResult::Continue(mutation, bundle, data))
    }
}
