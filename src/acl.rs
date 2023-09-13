use anyhow::Result;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

// Access Types
#[derive(Debug, Clone, PartialEq, PartialOrd, serde_derive::Deserialize)]
pub enum AccessType {
    Nothing,
    Read,
    Append,
    Modify,
}

pub trait AclChecker: Send + Sync + 'static {
    fn allowed(&self, user: &str, path: &str, tpe: &str, access: AccessType) -> bool;
}

// ACL for a repo
type RepoAcl = HashMap<&'static str, AccessType>;

// Acl holds ACLs for all repos
#[derive(Clone)]
pub struct Acl {
    repos: HashMap<String, RepoAcl>,
    append_only: bool,
    private_repo: bool,
}

// read_toml is a helper func that reads the given file in toml
// into a Hashmap mapping each user to the whole passwd line
fn read_toml(file_path: &PathBuf) -> Result<HashMap<String, RepoAcl>> {
    let s = fs::read_to_string(file_path)?;
    // make the contents static in memory
    let s = Box::leak(s.into_boxed_str());

    let mut repos: HashMap<String, RepoAcl> = toml::from_str(s)?;
    // copy key "default" into ""
    if let Some(default) = repos.get("default") {
        let default = default.clone();
        repos.insert("".to_owned(), default);
    }
    Ok(repos)
}

impl Acl {
    pub fn from_file(
        append_only: bool,
        private_repo: bool,
        file_path: Option<PathBuf>,
    ) -> Result<Self> {
        let repos = match file_path {
            Some(file_path) => read_toml(&file_path)?,
            None => HashMap::new(),
        };
        Ok(Self {
            append_only,
            private_repo,
            repos,
        })
    }
}

impl AclChecker for Acl {
    // allowed yields whether thes access to {path,tpe, access} is allowed by user
    fn allowed(&self, user: &str, path: &str, tpe: &str, access: AccessType) -> bool {
        // Access to locks is always treated as Read
        let access = if tpe == "locks" {
            AccessType::Read
        } else {
            access
        };

        match self.repos.get(path) {
            // We have ACLs for this repo, use them!
            Some(repo_acl) => match repo_acl.get(user) {
                Some(user_access) => user_access >= &access,
                None => false,
            },
            // Use standards defined by flags --private-repo and --append-only
            None => {
                (user == path || !self.private_repo)
                    && (access != AccessType::Modify || !self.append_only)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::AccessType::*;
    use super::*;

    #[test]
    fn allowed_flags() {
        let mut acl = Acl {
            repos: HashMap::new(),
            append_only: true,
            private_repo: true,
        };
        assert!(!acl.allowed("bob", "sam", "keys", Read));
        assert!(!acl.allowed("bob", "sam", "data", Read));
        assert!(!acl.allowed("bob", "sam", "data", Append));
        assert!(!acl.allowed("bob", "sam", "data", Modify));
        assert!(!acl.allowed("bob", "bob", "data", Modify));
        assert!(acl.allowed("bob", "bob", "locks", Modify));
        assert!(acl.allowed("bob", "bob", "keys", Append));
        assert!(acl.allowed("bob", "bob", "data", Append));
        assert!(acl.allowed("", "", "data", Append));
        assert!(!acl.allowed("bob", "", "data", Read));

        acl.append_only = false;
        assert!(!acl.allowed("bob", "sam", "data", Modify));
        assert!(acl.allowed("bob", "bob", "data", Modify));

        acl.private_repo = false;
        assert!(acl.allowed("bob", "sam", "data", Modify));
        assert!(acl.allowed("bob", "bob", "data", Modify));
        assert!(acl.allowed("bob", "", "data", Modify));
    }

    #[test]
    fn repo_acl() {
        let mut acl = Acl {
            repos: HashMap::new(),
            append_only: true,
            private_repo: true,
        };

        let mut acl_all = HashMap::new();
        acl_all.insert("bob", Modify);
        acl_all.insert("sam", Append);
        acl_all.insert("paul", Read);
        acl.repos.insert("all".to_owned(), acl_all);

        let mut acl_bob = HashMap::new();
        acl_bob.insert("bob", Modify);
        acl.repos.insert("bob".to_owned(), acl_bob);

        let mut acl_sam = HashMap::new();
        acl_sam.insert("sam", Append);
        acl_sam.insert("bob", Read);
        acl.repos.insert("sam".to_owned(), acl_sam);

        // test ACLs for repo all
        assert!(acl.allowed("bob", "all", "keys", Modify));
        assert!(!acl.allowed("sam", "all", "keys", Modify));
        assert!(acl.allowed("sam", "all", "keys", Append));
        assert!(acl.allowed("sam", "all", "locks", Modify));
        assert!(!acl.allowed("paul", "all", "data", Append));
        assert!(acl.allowed("paul", "all", "data", Read));
        assert!(acl.allowed("paul", "all", "locks", Modify));
        assert!(!acl.allowed("attack", "all", "data", Modify));

        // test ACLs for repo bob
        assert!(acl.allowed("bob", "bob", "data", Modify));
        assert!(!acl.allowed("sam", "bob", "data", Read));
        assert!(!acl.allowed("attack", "bob", "locks", Modify));

        // test ACLs for repo sam
        assert!(!acl.allowed("sam", "sam", "data", Modify));
        assert!(acl.allowed("sam", "sam", "data", Append));
        assert!(!acl.allowed("bob", "sam", "keys", Append));
        assert!(acl.allowed("bob", "sam", "keys", Read));
        assert!(!acl.allowed("attack", "sam", "locks", Read));

        // test ACLs for repo paul => fall back to flags
        assert!(!acl.allowed("paul", "paul", "data", Modify));
        assert!(acl.allowed("paul", "paul", "data", Append));
        assert!(!acl.allowed("sam", "paul", "data", Read));
    }
}
