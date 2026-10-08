use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestKind {
    Sync,
    Retry,
    Release,
    Adopt,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    pub kind: RequestKind,
    #[serde(default)]
    pub pr: Option<String>,
}

pub enum Entry {
    Valid(PathBuf, Request),
    Invalid(PathBuf, String),
}

pub fn write(dir: &Path, kind: RequestKind, pr: Option<String>) -> Result<String> {
    std::fs::create_dir_all(dir)?;
    let id = uuid::Uuid::new_v4().to_string();
    let req = Request { id: id.clone(), kind, pr };
    let tmp = dir.join(format!(".{id}.tmp"));
    std::fs::write(&tmp, serde_json::to_vec(&req)?)?;
    std::fs::rename(&tmp, dir.join(format!("{id}.json")))?;
    Ok(id)
}

pub fn list(dir: &Path) -> Result<Vec<Entry>> {
    let rd = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e.into()),
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| Some((std::fs::metadata(&p).ok()?.modified().ok()?, p)))
        .collect();
    files.sort();
    Ok(files
        .into_iter()
        .map(|(_, p)| {
            let parsed = std::fs::read(&p)
                .map_err(|e| e.to_string())
                .and_then(|b| serde_json::from_slice::<Request>(&b).map_err(|e| e.to_string()));
            match parsed {
                Ok(r) => Entry::Valid(p, r),
                Err(e) => Entry::Invalid(p, e),
            }
        })
        .collect())
}

pub fn ack(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_list_ack_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let reqs = dir.path().join("requests");
        let id1 = write(&reqs, RequestKind::Sync, None).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(15));
        let id2 = write(&reqs, RequestKind::Release, Some("o/r#7".into())).unwrap();
        let entries = list(&reqs).unwrap();
        let ids: Vec<String> = entries
            .iter()
            .map(|e| match e { Entry::Valid(_, r) => r.id.clone(), Entry::Invalid(_, m) => panic!("{m}") })
            .collect();
        assert_eq!(ids, vec![id1, id2]);
        if let Entry::Valid(p, r) = &entries[1] {
            assert_eq!(r.kind, RequestKind::Release);
            assert_eq!(r.pr.as_deref(), Some("o/r#7"));
            ack(p).unwrap();
            ack(p).unwrap(); // idempotente
        }
        assert_eq!(list(&reqs).unwrap().len(), 1);
    }

    #[test]
    fn invalid_and_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lixo.json"), "não é json").unwrap();
        std::fs::write(dir.path().join(".abc.tmp"), "{}").unwrap();
        let entries = list(dir.path()).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(matches!(&entries[0], Entry::Invalid(_, _)));
    }

    #[test]
    fn missing_dir_is_empty() {
        assert!(list(std::path::Path::new("/nao/existe/requests")).unwrap().is_empty());
    }
}
