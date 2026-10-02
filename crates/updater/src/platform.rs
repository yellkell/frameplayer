//! Host facts the updater needs: CPU arch and SteamOS version.

/// Arch name used in manifests (`std::env::consts::ARCH`, e.g. `aarch64`).
pub fn arch() -> &'static str {
    std::env::consts::ARCH
}

/// Parsed subset of `/etc/os-release`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OsRelease {
    pub id: Option<String>,
    pub version_id: Option<String>,
    pub build_id: Option<String>,
    pub variant_id: Option<String>,
}

impl OsRelease {
    pub fn parse(text: &str) -> Self {
        let mut out = Self::default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let v = v.trim().trim_matches('"').trim_matches('\'').to_string();
            match k.trim() {
                "ID" => out.id = Some(v),
                "VERSION_ID" => out.version_id = Some(v),
                "BUILD_ID" => out.build_id = Some(v),
                "VARIANT_ID" => out.variant_id = Some(v),
                _ => {}
            }
        }
        out
    }

    pub fn read() -> Option<Self> {
        std::fs::read_to_string("/etc/os-release")
            .ok()
            .map(|t| Self::parse(&t))
    }

    pub fn is_steamos(&self) -> bool {
        self.id.as_deref() == Some("steamos")
    }
}

/// SteamOS `VERSION_ID`, or `None` when not running on SteamOS.
// [verify] On the Frame's ARM branch `VERSION_ID` may follow its own
// numbering (press coverage says "SteamOS 0.3.0" for the Frame update) rather
// than the Deck's 3.x. `min_steamos` in manifests must use whatever the
// device reports here; check `/etc/os-release` on hardware.
pub fn steamos_version() -> Option<String> {
    OsRelease::read()
        .filter(OsRelease::is_steamos)
        .and_then(|o| o.version_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_steamos_os_release() {
        let o = OsRelease::parse(
            "NAME=\"SteamOS\"\nID=steamos\nID_LIKE=arch\nVERSION_ID=3.8.0\nBUILD_ID=20260901.1\nVARIANT_ID=steamdeck\n# c\n",
        );
        assert!(o.is_steamos());
        assert_eq!(o.version_id.as_deref(), Some("3.8.0"));
        assert_eq!(o.build_id.as_deref(), Some("20260901.1"));
        assert!(!OsRelease::parse("ID=debian\n").is_steamos());
    }

    #[test]
    fn arch_nonempty() {
        assert!(!arch().is_empty());
    }
}
