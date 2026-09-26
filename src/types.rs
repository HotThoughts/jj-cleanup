//! Core identifier newtypes and the jj revset type.
//!
//! Every jj/GitHub identifier is a distinct type so a commit hash can never be used where a
//! bookmark name or a repository name is expected. They are plain `String` wrappers rather than an
//! external newtype crate, to keep the dependency surface small.

use std::fmt::{self, Display};

/// Defines a `String`-backed identifier newtype.
macro_rules! id_newtype {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Wraps a string as this identifier.
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            /// Borrows the underlying string.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }
    };
}

id_newtype! {
    /// A `git`/`jj` commit SHA. Also used for GitHub commit OIDs.
    CommitId
}

id_newtype! {
    /// A bookmark name (jj's equivalent of a git branch).
    Bookmark
}

id_newtype! {
    /// A git remote name, e.g. `origin`.
    Remote
}

id_newtype! {
    /// A GitHub owner (user or organization), e.g. `HotThoughts`.
    Owner
}

id_newtype! {
    /// A GitHub repository name, e.g. `jj-cleanup`.
    Repo
}

/// A jj revset expression, ready to be passed to `-r`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revset(String);

impl Revset {
    /// Wraps an already-valid revset expression.
    pub fn new(expression: impl Into<String>) -> Self {
        Self(expression.into())
    }

    /// Borrows the expression.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for Revset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
