use std::path::{Path, PathBuf};

/// Pure core: decides the corpus path from inputs. `required` forces a hard
/// error (not a skip) when data is missing. `var` names the environment
/// variable the caller reads, so the message points at that variable and not
/// at whichever one this helper happened to be written for first.
pub fn resolve_corpus(
    var: &str,
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
                "{var} points at a missing directory: {}",
                p.display()
            ));
        }
        return Ok(None);
    }
    if required {
        return Err(format!("{var}_REQUIRED=1 but {var} is unset"));
    }
    Ok(None)
}

/// Reads real env for one corpus variable. Panics loudly in required mode so a
/// misconfigured corpus job fails instead of silently passing. Callers still
/// `.expect()` the `None`, so neither mode can turn into a silent skip.
fn root_for(var: &str) -> Option<PathBuf> {
    let required = std::env::var(format!("{var}_REQUIRED")).ok().as_deref() == Some("1");
    match resolve_corpus(var, std::env::var(var).ok(), required, |p| p.is_dir()) {
        Ok(v) => v,
        Err(e) => panic!("{e}"),
    }
}

/// The public sample corpus (`XAERO_CORPUS`), shared by most corpus tests.
pub fn corpus_root() -> Option<PathBuf> {
    root_for("XAERO_CORPUS")
}

/// The private legacy archive (`XAERO_LEGACY_CORPUS`). Same contract as
/// [`corpus_root`]; it is a separate variable because the two data sets have
/// different availability, not because they need different rules.
pub fn legacy_corpus_root() -> Option<PathBuf> {
    root_for("XAERO_LEGACY_CORPUS")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn required_but_absent_is_an_error() {
        let r = resolve_corpus("XAERO_CORPUS", None, true, |_| false);
        assert!(r.is_err(), "required + no data must fail, not skip");
    }
    #[test]
    fn absent_and_not_required_is_none() {
        assert_eq!(
            resolve_corpus("XAERO_CORPUS", None, false, |_| false).unwrap(),
            None
        );
    }
    #[test]
    fn explicit_env_dir_is_used() {
        let r = resolve_corpus("XAERO_CORPUS", Some("/x".into()), false, |p| {
            p == std::path::Path::new("/x")
        });
        assert_eq!(r.unwrap(), Some(PathBuf::from("/x")));
    }
    #[test]
    fn the_message_names_the_variable_the_caller_reads() {
        let e = resolve_corpus("XAERO_LEGACY_CORPUS", None, true, |_| false).unwrap_err();
        assert!(
            e.contains("XAERO_LEGACY_CORPUS_REQUIRED") && !e.contains("XAERO_CORPUS_REQUIRED=1"),
            "message must name the legacy variable, got: {e}"
        );
    }
}
