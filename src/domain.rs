//! Shared provider-independent models. No credentials belong in these values.
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Account {
    pub host: String,
    pub login: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Repository {
    pub host: String,
    pub owner: String,
    pub name: String,
    pub account: Account,
    pub local_path: Option<PathBuf>,
}
impl Repository {
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
    pub fn cache_key(&self) -> String {
        // JSON encoding preserves separators inside fields and account isolation.
        serde_json::to_string(&(
            self.host.as_str(),
            self.owner.as_str(),
            self.name.as_str(),
            self.account.host.as_str(),
            self.account.login.as_str(),
        ))
        .expect("string tuple")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revision {
    pub base_sha: String,
    pub head_sha: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub source_branch: String,
    pub target_branch: String,
    pub author: String,
    pub reviewers: Vec<String>,
    pub assignees: Vec<String>,
    pub labels: Vec<String>,
    pub draft: bool,
    /// OPEN, CLOSED, MERGED.
    pub state: String,
    pub review_status: String,
    pub check_status: String,
    pub base_sha: String,
    pub head_sha: String,
    pub url: String,
}
impl PullRequest {
    pub fn revision(&self) -> Revision {
        Revision {
            base_sha: self.base_sha.clone(),
            head_sha: self.head_sha.clone(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChangedFile {
    pub path: String,
    pub previous_path: Option<String>,
    pub status: String,
    pub additions: u64,
    pub deletions: u64,
    /// None means unavailable/binary/truncated, never an empty complete patch.
    pub patch: Option<String>,
    pub patch_complete: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Comparison {
    pub revision: Revision,
    pub files: Vec<ChangedFile>,
    pub complete: bool,
    pub notice: Option<String>,
}
