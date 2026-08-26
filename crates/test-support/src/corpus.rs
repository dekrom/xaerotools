use std::path::{Path, PathBuf};

/// Pure core: decides the corpus path from inputs. `required` forces a hard
/// error (not a skip) when data is missing (set via XAERO_CORPUS_REQUIRED).
pub fn resolve_corpus(
    env_value: Option<String>,
    required: bool,
    directory_exists: impl Fn(&Path) -> bool,
) -> Result<Option<PathBuf>, String> {
    if let Some(p) = env_value {
        let p = PathBuf::from(p);
        if directory_exists(&p) {
            return Ok(Some(p));
        }
        if required {
            return Err(format!(
                "XAERO_CORPUS points at a missing directory: {}",
                p.display()
            ));
        }
        return Ok(None);
    }
    if required {
        return Err("XAERO_CORPUS_REQUIRED=1 but XAERO_CORPUS is unset".into());
    }
    Ok(None)
}

/// Test-facing resolver. Reads real env. Panics loudly in required mode so a
/// misconfigured corpus job fails instead of silently passing.
pub fn corpus_root() -> Option<PathBuf> {
    let required = std::env::var("XAERO_CORPUS_REQUIRED").ok().as_deref() == Some("1");
    match resolve_corpus(std::env::var("XAERO_CORPUS").ok(), required, |p| p.is_dir()) {
        Ok(v) => v,
        Err(e) => panic!("{e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn required_but_absent_is_an_error() {
        let r = resolve_corpus(None, true, |_| false);
        assert!(r.is_err(), "required + no data must fail, not skip");
    }
    #[test]
    fn absent_and_not_required_is_none() {
        assert_eq!(resolve_corpus(None, false, |_| false).unwrap(), None);
    }
    #[test]
    fn explicit_env_dir_is_used() {
        let r = resolve_corpus(Some("/x".into()), false, |p| {
            p == std::path::Path::new("/x")
        });
        assert_eq!(r.unwrap(), Some(PathBuf::from("/x")));
    }
}
