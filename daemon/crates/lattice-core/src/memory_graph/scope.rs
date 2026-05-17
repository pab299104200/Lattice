use rusqlite::types::Value;
use std::fmt;

use super::MemoryScope;

#[derive(Clone, Debug, PartialEq)]
pub struct ScopePredicate {
    pub where_clause: String,
    pub bind_values: Vec<Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScopeFilter {
    clauses: Vec<ScopeClause>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScopeClause {
    Session {
        session_id: String,
    },
    Branch {
        workspace_id: String,
        branch_ref: String,
    },
    Repo {
        workspace_id: String,
    },
    User {
        user_id: String,
    },
    Organization {
        org_id: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScopeError {
    Underspecified,
    CrossScopeViolation {
        requested: MemoryScope,
        allowed: MemoryScope,
    },
    IdentityMismatch {
        field: &'static str,
    },
}

impl fmt::Display for ScopeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScopeError::Underspecified => formatter.write_str("scope filter is underspecified"),
            ScopeError::CrossScopeViolation { requested, allowed } => write!(
                formatter,
                "requested scope `{}` is outside allowed scope `{}`",
                requested.as_str(),
                allowed.as_str()
            ),
            ScopeError::IdentityMismatch { field } => {
                write!(formatter, "scope identity field `{field}` does not match")
            }
        }
    }
}

impl std::error::Error for ScopeError {}

impl ScopeFilter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn session(session_id: impl Into<String>) -> Self {
        Self::new().with_session(session_id)
    }

    pub fn branch(workspace_id: impl Into<String>, branch_ref: impl Into<String>) -> Self {
        Self::new().with_branch(workspace_id, branch_ref)
    }

    pub fn repo(workspace_id: impl Into<String>) -> Self {
        Self::new().with_repo(workspace_id)
    }

    pub fn user(user_id: impl Into<String>) -> Self {
        Self::new().with_user(user_id)
    }

    pub fn organization(org_id: impl Into<String>) -> Self {
        Self::new().with_organization(org_id)
    }

    pub fn with_session(mut self, session_id: impl Into<String>) -> Self {
        self.push_unique(ScopeClause::Session {
            session_id: session_id.into(),
        });
        self
    }

    pub fn with_branch(
        mut self,
        workspace_id: impl Into<String>,
        branch_ref: impl Into<String>,
    ) -> Self {
        self.push_unique(ScopeClause::Branch {
            workspace_id: workspace_id.into(),
            branch_ref: branch_ref.into(),
        });
        self
    }

    pub fn with_repo(mut self, workspace_id: impl Into<String>) -> Self {
        self.push_unique(ScopeClause::Repo {
            workspace_id: workspace_id.into(),
        });
        self
    }

    pub fn with_user(mut self, user_id: impl Into<String>) -> Self {
        self.push_unique(ScopeClause::User {
            user_id: user_id.into(),
        });
        self
    }

    pub fn with_organization(mut self, org_id: impl Into<String>) -> Self {
        self.push_unique(ScopeClause::Organization {
            org_id: org_id.into(),
        });
        self
    }

    pub fn clauses(&self) -> &[ScopeClause] {
        &self.clauses
    }

    pub fn is_empty(&self) -> bool {
        self.clauses.is_empty()
    }

    pub fn to_sql_predicate(&self) -> ScopePredicate {
        if self.is_empty() {
            return ScopePredicate {
                where_clause: "1 = 0".to_string(),
                bind_values: Vec::new(),
            };
        }

        let mut predicates = Vec::with_capacity(self.clauses.len());
        let mut bind_values = Vec::new();

        for clause in &self.clauses {
            match clause {
                ScopeClause::Session { session_id } => {
                    predicates.push("(scope = ? AND scope_session_id = ?)".to_string());
                    bind_values.push(Value::Text(MemoryScope::Session.as_str().to_string()));
                    bind_values.push(Value::Text(session_id.clone()));
                }
                ScopeClause::Branch {
                    workspace_id,
                    branch_ref,
                } => {
                    predicates.push(
                        "(scope = ? AND scope_workspace_id = ? AND scope_branch = ?)".to_string(),
                    );
                    bind_values.push(Value::Text(MemoryScope::Branch.as_str().to_string()));
                    bind_values.push(Value::Text(workspace_id.clone()));
                    bind_values.push(Value::Text(branch_ref.clone()));
                }
                ScopeClause::Repo { workspace_id } => {
                    predicates.push("(scope = ? AND scope_workspace_id = ?)".to_string());
                    bind_values.push(Value::Text(MemoryScope::Repo.as_str().to_string()));
                    bind_values.push(Value::Text(workspace_id.clone()));
                }
                ScopeClause::User { user_id } => {
                    predicates.push("(scope = ? AND scope_user_id = ?)".to_string());
                    bind_values.push(Value::Text(MemoryScope::User.as_str().to_string()));
                    bind_values.push(Value::Text(user_id.clone()));
                }
                ScopeClause::Organization { org_id } => {
                    predicates.push("(scope = ? AND scope_org_id = ?)".to_string());
                    bind_values.push(Value::Text(MemoryScope::Organization.as_str().to_string()));
                    bind_values.push(Value::Text(org_id.clone()));
                }
            }
        }

        ScopePredicate {
            where_clause: format!("({})", predicates.join(" OR ")),
            bind_values,
        }
    }

    pub fn enforce_subset(&self, allowed: &ScopeFilter) -> Result<(), ScopeError> {
        if self.is_empty() {
            return Err(ScopeError::Underspecified);
        }

        for requested in &self.clauses {
            let requested_scope = requested.scope();
            let Some(candidate) = allowed
                .clauses
                .iter()
                .find(|allowed_clause| allowed_clause.scope() == requested_scope)
            else {
                return Err(ScopeError::CrossScopeViolation {
                    requested: requested_scope,
                    allowed: allowed.primary_scope().unwrap_or(requested_scope),
                });
            };

            match (requested, candidate) {
                (
                    ScopeClause::Session {
                        session_id: requested,
                    },
                    ScopeClause::Session {
                        session_id: allowed,
                    },
                ) if requested != allowed => {
                    return Err(ScopeError::IdentityMismatch {
                        field: "scope_session_id",
                    });
                }
                (
                    ScopeClause::Branch {
                        workspace_id: requested_workspace,
                        branch_ref: requested_branch,
                    },
                    ScopeClause::Branch {
                        workspace_id: allowed_workspace,
                        branch_ref: allowed_branch,
                    },
                ) => {
                    if requested_workspace != allowed_workspace {
                        return Err(ScopeError::IdentityMismatch {
                            field: "scope_workspace_id",
                        });
                    }
                    if requested_branch != allowed_branch {
                        return Err(ScopeError::IdentityMismatch {
                            field: "scope_branch",
                        });
                    }
                }
                (
                    ScopeClause::Repo {
                        workspace_id: requested,
                    },
                    ScopeClause::Repo {
                        workspace_id: allowed,
                    },
                ) if requested != allowed => {
                    return Err(ScopeError::IdentityMismatch {
                        field: "scope_workspace_id",
                    });
                }
                (
                    ScopeClause::User { user_id: requested },
                    ScopeClause::User { user_id: allowed },
                ) if requested != allowed => {
                    return Err(ScopeError::IdentityMismatch {
                        field: "scope_user_id",
                    });
                }
                (
                    ScopeClause::Organization { org_id: requested },
                    ScopeClause::Organization { org_id: allowed },
                ) if requested != allowed => {
                    return Err(ScopeError::IdentityMismatch {
                        field: "scope_org_id",
                    });
                }
                _ => {}
            }
        }

        Ok(())
    }

    fn push_unique(&mut self, clause: ScopeClause) {
        if !self.clauses.contains(&clause) {
            self.clauses.push(clause);
        }
    }

    fn primary_scope(&self) -> Option<MemoryScope> {
        self.clauses.first().map(ScopeClause::scope)
    }
}

impl ScopeClause {
    pub fn scope(&self) -> MemoryScope {
        match self {
            ScopeClause::Session { .. } => MemoryScope::Session,
            ScopeClause::Branch { .. } => MemoryScope::Branch,
            ScopeClause::Repo { .. } => MemoryScope::Repo,
            ScopeClause::User { .. } => MemoryScope::User,
            ScopeClause::Organization { .. } => MemoryScope::Organization,
        }
    }
}
