//! DRM render/display nodes and the kernel driver behind each.

use crate::util::{IOC_READWRITE, ioc};
use serde::Serialize;

#[repr(C)]
pub struct DrmVersion {
    pub version_major: i32,
    pub version_minor: i32,
    pub version_patchlevel: i32,
    pub name_len: usize,
    pub name: *mut libc::c_char,
    pub date_len: usize,
    pub date: *mut libc::c_char,
    pub desc_len: usize,
    pub desc: *mut libc::c_char,
}

pub const DRM_IOCTL_VERSION: u64 =
    ioc(IOC_READWRITE, b'd', 0x00, std::mem::size_of::<DrmVersion>());

#[derive(Serialize)]
pub struct DrmNode {
    pub node: String,
    pub driver: Option<String>,
    pub description: Option<String>,
    pub version: Option<String>,
    pub open_error: Option<String>,
}

pub fn probe() -> Vec<DrmNode> {
    let mut nodes: Vec<String> = std::fs::read_dir("/dev/dri")
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| format!("/dev/dri/{}", e.file_name().to_string_lossy()))
                .filter(|p| p.contains("card") || p.contains("renderD"))
                .collect()
        })
        .unwrap_or_default();
    nodes.sort();
    nodes.into_iter().map(|n| probe_node(&n)).collect()
}

fn probe_node(node: &str) -> DrmNode {
    let mut out = DrmNode {
        node: node.into(),
        driver: None,
        description: None,
        version: None,
        open_error: None,
    };
    let c = std::ffi::CString::new(node).expect("path");
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    if fd < 0 {
        out.open_error = Some(crate::util::last_os_error());
        return out;
    }
    let mut name = [0 as libc::c_char; 64];
    let mut desc = [0 as libc::c_char; 128];
    let mut date = [0 as libc::c_char; 32];
    let mut v = DrmVersion {
        version_major: 0,
        version_minor: 0,
        version_patchlevel: 0,
        name_len: name.len() - 1,
        name: name.as_mut_ptr(),
        date_len: date.len() - 1,
        date: date.as_mut_ptr(),
        desc_len: desc.len() - 1,
        desc: desc.as_mut_ptr(),
    };
    if unsafe { libc::ioctl(fd, DRM_IOCTL_VERSION as _, &mut v) } == 0 {
        out.driver = Some(crate::util::fixed_cstr(&name));
        out.description = Some(crate::util::fixed_cstr(&desc));
        out.version = Some(format!(
            "{}.{}.{}",
            v.version_major, v.version_minor, v.version_patchlevel
        ));
    } else {
        out.open_error = Some(format!(
            "DRM_IOCTL_VERSION: {}",
            crate::util::last_os_error()
        ));
    }
    unsafe { libc::close(fd) };
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn ioctl_number_matches_kernel_header() {
        // drm.h: DRM_IOWR(0x00, struct drm_version) on 64-bit.
        assert_eq!(super::DRM_IOCTL_VERSION, 0xc040_6400);
    }
}
