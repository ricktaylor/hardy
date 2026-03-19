use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Filter with name '{0}' already exists")]
    AlreadyExists(String),

    #[error("Filter dependency '{0}' not found")]
    DependencyNotFound(String),

    #[error("Filter '{0}' has dependants: {1:?}")]
    HasDependants(String, Vec<String>),
}

pub type Result<T> = core::result::Result<T, Error>;
