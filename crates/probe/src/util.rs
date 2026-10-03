//! Small helpers shared by the checks: result builder, file/user lookups,
//! `dlopen` probing, PATH search.

use crate::parse;
use crate::report::{CheckOutput, Finding, Status};
use serde::Serialize;
use serde_json::{Map, Value};
use std::ffi::{CStr, CString};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Accumulates findings and data for one check.
#[derive(Debug, Default, Clone)]
pub struct Out {
    pub findings: Vec<Finding>,
    pub data: Map<String, Value>,
}

impl Out {
    pub fn new() -> Out {
        Out::default()
    }

    pub fn finding(&mut self, id: &str, status: Status, refs: &[&str], summary: impl Into<String>) {
        self.findings.push(Finding {
            id: id.into(),
            status,
            summary: summary.into(),
            refs: refs.iter().map(|r| r.to_string()).collect(),
        });
    }

    pub fn set(&mut self, key: &str, v: impl Serialize) {
        self.data
            .insert(key.into(), serde_json::to_value(v).unwrap_or(Value::Null));
    }

    /// Status of the first finding with this id.
    pub fn status_of(&self, id: &str) -> Option<Status> {
        self.findings.iter().find(|f| f.id == id).map(|f| f.status)
    }

    pub fn snapshot(&self, status: Status, summary: impl Into<String>) -> CheckOutput {
        self.clone().finish(status, summary)
    }

    pub fn finish(self, status: Status, summary: impl Into<String>) -> CheckOutput {
        CheckOutput {
            status,
            summary: summary.into(),
            findings: self.findings,
            data: Value::Object(self.data),
        }
    }
}

/// File contents without trailing whitespace, if readable.
pub fn read_trim(path: impl AsRef<Path>) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim_end().to_string())
}

/// The login name of the current user.
pub fn username() -> Option<String> {
    // SAFETY: getuid never fails.
    let uid = unsafe { libc::getuid() };
    if let Some(name) = std::fs::read_to_string("/etc/passwd")
        .ok()
        .and_then(|s| parse::passwd_name(&s, uid))
    {
        return Some(name);
    }
    std::env::var("USER").ok().filter(|u| !u.is_empty())
}

/// `$HOME`, falling back to `/tmp`.
pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// First executable named `cmd` on `$PATH`.
pub fn which(cmd: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(cmd)).find(|p| {
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
}

/// Group name for a gid (from /etc/group), else the number.
pub fn group_name(gid: u32) -> String {
    std::fs::read_to_string("/etc/group")
        .ok()
        .and_then(|s| {
            parse::parse_group_file(&s)
                .into_iter()
                .find(|g| g.gid == gid)
                .map(|g| g.name)
        })
        .unwrap_or_else(|| gid.to_string())
}

/// Owner shown as a role rather than a name: `root`, `self` or `other`.
pub fn owner_role(uid: u32) -> &'static str {
    // SAFETY: getuid never fails.
    let me = unsafe { libc::getuid() };
    if uid == 0 {
        "root"
    } else if uid == me {
        "self"
    } else {
        "other"
    }
}

/// `access(2)` with the real uid.
pub fn can_access(path: &Path, mode: libc::c_int) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: valid C string.
    unsafe { libc::access(c.as_ptr(), mode) == 0 }
}

/// Compact description of a device node: type/permissions, owner role,
/// group and whether we can open it read-write.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct NodeInfo {
    pub path: String,
    pub mode: String,
    pub owner: &'static str,
    pub group: String,
    pub rw: bool,
}

pub fn node_info(path: &Path) -> Option<NodeInfo> {
    let m = std::fs::metadata(path).ok()?;
    Some(NodeInfo {
        path: path.display().to_string(),
        mode: parse::mode_string(m.mode()),
        owner: owner_role(m.uid()),
        group: group_name(m.gid()),
        rw: can_access(path, libc::R_OK | libc::W_OK),
    })
}

/// Sorted entries of `dir` whose file name starts with `prefix`.
pub fn list_prefixed(dir: &str, prefix: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with(prefix))
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort_by(|a, b| parse::natural_cmp(&a.to_string_lossy(), &b.to_string_lossy()));
    v
}

/// Result of trying to `dlopen` a shared library.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LibProbe {
    pub name: String,
    pub loaded: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `dlopen(name)` and report where the dynamic linker found it.
pub fn dlopen_probe(name: &str) -> LibProbe {
    let c = CString::new(name).expect("no NUL in library names");
    // SAFETY: plain dlopen of a system library; constructors run, which
    // is why risky probes run in a child process.
    let h = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if h.is_null() {
        // SAFETY: dlerror returns a thread-local string or NULL.
        let err = unsafe {
            let e = libc::dlerror();
            if e.is_null() {
                "dlopen failed".to_string()
            } else {
                CStr::from_ptr(e).to_string_lossy().into_owned()
            }
        };
        return LibProbe {
            name: name.into(),
            loaded: false,
            path: None,
            error: Some(err),
        };
    }
    let base = name.split(".so").next().unwrap_or(name);
    let path = std::fs::read_to_string("/proc/self/maps")
        .ok()
        .and_then(|maps| parse::mapped_library(&maps, base));
    // SAFETY: handle from dlopen above.
    unsafe { libc::dlclose(h) };
    LibProbe {
        name: name.into(),
        loaded: true,
        path,
        error: None,
    }
}

/// Seconds since the Unix epoch.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// glibc version string (`gnu_get_libc_version`).
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub fn glibc_version() -> Option<String> {
    // SAFETY: returns a static NUL-terminated string.
    unsafe {
        let p = libc::gnu_get_libc_version();
        (!p.is_null()).then(|| CStr::from_ptr(p).to_string_lossy().into_owned())
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub fn glibc_version() -> Option<String> {
    None
}

/// Plain TCP connect with a short timeout.
pub fn tcp_open(addr: std::net::SocketAddr, timeout: std::time::Duration) -> bool {
    std::net::TcpStream::connect_timeout(&addr, timeout).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn out_builder() {
        let mut o = Out::new();
        o.finding("a", Status::Pass, &["P1"], "ok");
        o.set("n", 3);
        assert_eq!(o.status_of("a"), Some(Status::Pass));
        assert_eq!(o.status_of("b"), None);
        let snap = o.snapshot(Status::Unknown, "half");
        assert_eq!(snap.summary, "half");
        let out = o.finish(Status::Pass, "done");
        assert_eq!(out.data["n"], 3);
        assert_eq!(out.findings[0].refs, vec!["P1".to_string()]);
    }

    #[test]
    fn dlopen_libc_and_missing() {
        let ok = dlopen_probe("libc.so.6");
        assert!(ok.loaded);
        assert!(ok.path.as_deref().unwrap_or("").contains("libc"), "{ok:?}");
        let missing = dlopen_probe("libdefinitely-not-here.so.9");
        assert!(!missing.loaded);
        assert!(missing.error.is_some());
    }

    #[test]
    fn misc() {
        assert!(which("sh").is_some());
        assert!(which("no-such-binary-fp").is_none());
        assert_eq!(owner_role(0), "root");
        assert!(glibc_version().is_some_and(|v| v.starts_with("2.")));
        assert!(node_info(Path::new("/dev/null")).is_some_and(|n| n.mode.starts_with('c')));
    }
}
