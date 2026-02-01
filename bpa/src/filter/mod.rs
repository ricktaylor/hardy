use super::*;
use hardy_bpv7::status_report::ReasonCode;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("The filter has a circular dependency on {0}")]
    CircularDependency(String),
}

#[derive(Debug, Default)]
pub enum FilterResult {
    #[default]
    Continue,

    Drop(Option<ReasonCode>),
}

#[derive(Debug)]
pub enum RewriteResult {
    Continue(Option<Box<[u8]>>),

    Drop(Option<ReasonCode>),
}

impl Default for RewriteResult {
    fn default() -> Self {
        Self::Continue(None)
    }
}

#[async_trait]
pub trait Filter {
    fn filter(&self, bundle: &bundle::Bundle, data: &[u8]) -> FilterResult;

    fn rewrite(&self, bundle: &bundle::Bundle, data: &[u8]) -> RewriteResult;
}

#[async_trait]
pub trait Rewriter {}

// TODO: This needs to be implemented in a 'registry' just like CLAs etc, and added to the Bpa struct
pub fn register_filter(
    _name: &str,
    _after: &[&str],
    _rewriter: bool,
    _filter: Arc<dyn Filter>,
) -> Result<Option<Arc<dyn Filter>>, Error> {
    // Add the new filter after all the filters specified in `after`
    // Returns any previously registered filter with the same name

    // TODO: Perform the dep

    todo!()
}
