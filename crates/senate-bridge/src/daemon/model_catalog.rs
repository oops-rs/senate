//! Resolve a family only when its matches differ by numeric version alone.
use std::path::Path;

use crate::ipc::IpcError;

pub(super) async fn agy_models(program: &Path, cwd: &Path) -> Result<Vec<String>, IpcError> {
    let output = tokio::process::Command::new(program)
        .arg("models")
        .current_dir(cwd)
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(std::time::Duration::from_secs(10), output)
        .await
        .map_err(|_| IpcError::internal("agy models timed out"))?
        .map_err(IpcError::internal)?;
    if !output.status.success() {
        return Err(IpcError::internal(format!(
            "agy models exited with {}",
            output.status
        )));
    }
    let stdout = std::str::from_utf8(&output.stdout).map_err(IpcError::internal)?;
    // agy also prints a progress banner. Only its ID<TAB>label rows are models.
    Ok(stdout
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(id, _)| id.trim().to_owned())
        .filter(|id| !id.is_empty())
        .collect())
}

fn segments(id: &str) -> impl Iterator<Item = &str> {
    id.split(['-', '_', '.']).filter(|part| !part.is_empty())
}

fn family_version(id: &str) -> (Vec<&str>, Vec<u64>) {
    let mut family = Vec::new();
    let mut version = Vec::new();
    let mut in_version = false;
    for segment in segments(id) {
        if let Ok(n) = segment.parse::<u64>() {
            if !in_version {
                family.push("");
            }
            version.push(n);
            in_version = true;
        } else {
            family.push(segment);
            in_version = false;
        }
    }
    (family, version)
}

pub(super) fn resolve(request: &str, mut catalog: Vec<String>) -> Result<String, IpcError> {
    if catalog.iter().any(|id| id == request) {
        return Ok(request.to_owned());
    }
    if catalog.is_empty() {
        return Err(IpcError::bad_request(format!(
            "no models available for {request:?}: runtime returned an empty catalog"
        )));
    }
    catalog.sort();
    catalog.dedup();
    let matches: Vec<_> = catalog
        .iter()
        .filter(|id| segments(id).any(|s| s == request))
        .collect();
    let Some(first) = matches.first() else {
        catalog.sort_by_cached_key(|id| {
            segments(id)
                .chain(std::iter::once(id.as_str()))
                .map(|s| strsim::damerau_levenshtein(s, request))
                .min()
        });
        catalog.truncate(5);
        return Err(IpcError::bad_request(format!(
            "unknown model {request:?}; close matches: {}",
            catalog.join(", ")
        )));
    };
    let (family, mut newest) = family_version(first);
    let mut best = *first;
    let mut tied = false;
    for id in matches.iter().skip(1) {
        let (other, version) = family_version(id);
        if other != family {
            tied = true;
            break;
        }
        match version.cmp(&newest) {
            std::cmp::Ordering::Greater => {
                best = id;
                newest = version;
                tied = false;
            }
            std::cmp::Ordering::Equal => tied = true,
            std::cmp::Ordering::Less => {}
        }
    }
    if tied {
        let candidates = matches
            .iter()
            .map(|id| id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(IpcError::bad_request(format!(
            "ambiguous model {request:?}; candidates: {candidates}"
        )));
    }
    Ok(best.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn family_resolution() {
        let choose =
            |name, ids: &[&str]| resolve(name, ids.iter().map(|s| s.to_string()).collect());
        let versions = ["gpt-6.2-astra", "gpt-6.10-astra", "gpt-6-astra"];
        assert_eq!(choose("astra", &versions).unwrap(), versions[1]);
        assert_eq!(choose(versions[0], &versions).unwrap(), versions[0]);
        assert_eq!(choose("astra", &["astra", versions[0]]).unwrap(), "astra");
        let unknown = choose("astr", &versions).unwrap_err().to_string();
        assert!(unknown.contains("unknown model") && unknown.contains(versions[0]));
        let ambiguous = choose("astra", &[versions[0], "gpt-7-astra-mini"])
            .unwrap_err()
            .to_string();
        assert!(ambiguous.contains(versions[0]) && ambiguous.contains("gpt-7-astra-mini"));
    }
}
