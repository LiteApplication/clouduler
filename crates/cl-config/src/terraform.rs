//! Static checks on a stored OpenTofu module.

/// Parses `source` as HCL and rejects syntax errors plus any `terraform`
/// block holding a `backend` or `cloud` block: state lives in raft, so a
/// module may not point it elsewhere.
pub fn validate_module(source: &str) -> Result<(), String> {
    let body = hcl::parse(source).map_err(|e| format!("invalid HCL: {e}"))?;
    for block in body.blocks() {
        if block.identifier() != "terraform" {
            continue;
        }
        for inner in block.body().blocks() {
            let id = inner.identifier();
            if id == "backend" || id == "cloud" {
                return Err(format!(
                    "terraform {{ {id} }} blocks are not allowed: clouduler keeps the state itself"
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_a_plain_module() {
        validate_module(
            "terraform {\n  required_providers {\n    http = { source = \"hashicorp/http\" }\n  }\n}\nvariable \"x\" {\n  sensitive = true\n}\n",
        )
        .unwrap();
        validate_module("").unwrap();
    }

    #[test]
    fn rejects_syntax_errors() {
        assert!(validate_module("variable \"x\" {").is_err());
    }

    #[test]
    fn rejects_backend_and_cloud() {
        let e = validate_module("terraform {\n  backend \"s3\" {}\n}\n").unwrap_err();
        assert!(e.contains("backend"), "{e}");
        let e = validate_module("terraform {\n  cloud {\n    organization = \"o\"\n  }\n}\n")
            .unwrap_err();
        assert!(e.contains("cloud"), "{e}");
    }
}
