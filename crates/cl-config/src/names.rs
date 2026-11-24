//! Name and hostname predicates shared by the cluster and stack validators.

/// A stack name ends up in container names, overlay network names and the git
/// backup's `stacks/<name>.yaml`: lowercase letters, digits, `-` and `_`,
/// starting and ending alphanumeric, at most 63 bytes. `_` is allowed for
/// existing stacks; the `<stack>_<network>` ambiguity is handled by
/// `cl_sched::run::overlay_collision`.
pub fn validate_stack_name(name: &str) -> std::result::Result<(), String> {
    let invalid = |why: &str| Err(format!("invalid stack name {name:?}: {why}"));
    if name.is_empty() || name.len() > 63 {
        return invalid("must be 1 to 63 bytes long");
    }
    let edge_ok = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    if !name.starts_with(edge_ok) || !name.ends_with(edge_ok) {
        return invalid("must start and end with a lowercase letter or digit");
    }
    if !name.chars().all(|c| edge_ok(c) || c == '-' || c == '_') {
        return invalid("only lowercase letters, digits, `-` and `_` are allowed");
    }
    Ok(())
}

pub(crate) fn is_dns_label(s: &str) -> bool {
    if s.is_empty() || s.len() > 63 {
        return false;
    }
    if s.starts_with('-') || s.ends_with('-') {
        return false;
    }
    s.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Same rules as raft's object names (they become file names): 1-253 characters of
/// letters, digits, `.`, `-`, `_`, not starting with a dot.
pub fn check_object_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 253 {
        return Err(format!("{name:?} must be between 1 and 253 characters"));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
    {
        return Err(format!(
            "{name:?} may only contain letters, digits, dot, dash and underscore"
        ));
    }
    if name.starts_with('.') {
        return Err(format!("{name:?} must not start with a dot"));
    }
    Ok(())
}

/// True when `name` is the domain itself or a subdomain of it.
pub(crate) fn is_within(name: &str, domain: &str) -> bool {
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    let domain = domain.trim_end_matches('.').to_ascii_lowercase();
    name == domain || name.ends_with(&format!(".{domain}"))
}
