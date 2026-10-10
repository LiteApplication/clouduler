//! The documentation and example stacks an AI client can read before writing
//! a stack, compiled into the binary so they always match the running
//! version and need no file access on the node.

pub struct Doc {
    pub uri: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub mime: &'static str,
    pub text: &'static str,
}

pub const DOCS: &[Doc] = &[
    Doc {
        uri: "clouduler://docs/stacks",
        name: "docs/stacks.md",
        description: "Stack file format: services, volumes, configs, secrets, placement. Read before writing a stack.",
        mime: "text/markdown",
        text: include_str!("../../../../docs/stacks.md"),
    },
    Doc {
        uri: "clouduler://docs/networking",
        name: "docs/networking.md",
        description: "Internal, private and public networks, and how services reach each other and the outside.",
        mime: "text/markdown",
        text: include_str!("../../../../docs/networking.md"),
    },
    Doc {
        uri: "clouduler://docs/secrets",
        name: "docs/secrets.md",
        description: "How secrets are stored and mounted, and how to reference them from a stack.",
        mime: "text/markdown",
        text: include_str!("../../../../docs/secrets.md"),
    },
    Doc {
        uri: "clouduler://docs/proxy",
        name: "docs/proxy.md",
        description: "Publishing a service through the built-in reverse proxy: hosts, TLS, health checks.",
        mime: "text/markdown",
        text: include_str!("../../../../docs/proxy.md"),
    },
    Doc {
        uri: "clouduler://examples/simple.yaml",
        name: "examples/stacks/simple.yaml",
        description: "A minimal working stack file.",
        mime: "application/yaml",
        text: include_str!("../../../../examples/stacks/simple.yaml"),
    },
    Doc {
        uri: "clouduler://examples/gitlab.yaml",
        name: "examples/stacks/gitlab.yaml",
        description: "A larger stack with several services, volumes, secrets and a published route.",
        mime: "application/yaml",
        text: include_str!("../../../../examples/stacks/gitlab.yaml"),
    },
];

pub fn find(uri: &str) -> Option<&'static Doc> {
    DOCS.iter().find(|d| d.uri == uri)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_resource_has_content_and_a_unique_uri() {
        let mut seen = std::collections::HashSet::new();
        for d in DOCS {
            assert!(!d.text.is_empty(), "{}", d.uri);
            assert!(seen.insert(d.uri), "duplicate {}", d.uri);
        }
    }
}
